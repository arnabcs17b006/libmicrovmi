use std::convert::TryInto;
use std::error::Error;
use std::mem;
use std::vec::Vec;

use kvmi::{
    KVMIntrospectable, KVMiCr, KVMiEvent, KVMiEventReply, KVMiEventType, KVMiInterceptType,
};

use crate::api::{
    CrType, Event, EventReplyType, EventType, InterceptType, Introspectable, Registers,
    X86Registers,
};

#[derive(Debug)]
pub struct Kvm<T: KVMIntrospectable> {
    kvmi: T,
    expect_pause_ev: u32,
    // VCPU -> KVMiEvent
    vec_events: Vec<Option<KVMiEvent>>,
}

impl<T: KVMIntrospectable> Kvm<T> {
    pub fn new(domain_name: &str, mut kvmi: T) -> Result<Self, Box<dyn Error>> {
        let socket_path = "/tmp/introspector";
        debug!("init on {} (socket: {})", domain_name, socket_path);
        kvmi.init(socket_path)?;
        let mut kvm = Kvm {
            kvmi,
            expect_pause_ev: 0,
            vec_events: Vec::new(),
        };

        // set vec_events size
        let vcpu_count = kvm.get_vcpu_count().unwrap();
        kvm.vec_events
            .resize_with(vcpu_count.try_into().unwrap(), || None);

        // enable CR event intercept by default
        // (interception will take place when CR register will be specified)
        for vcpu in 0..vcpu_count {
            kvm.kvmi
                .control_events(vcpu, KVMiInterceptType::Cr, true)
                .unwrap();
        }

        Ok(kvm)
    }
}

impl<T: KVMIntrospectable> Introspectable for Kvm<T> {
    fn get_vcpu_count(&self) -> Result<u16, Box<dyn Error>> {
        Ok(self.kvmi.get_vcpu_count().unwrap().try_into()?)
    }

    fn read_physical(&self, paddr: u64, buf: &mut [u8]) -> Result<(), Box<dyn Error>> {
        Ok(self.kvmi.read_physical(paddr, buf)?)
    }

    fn get_max_physical_addr(&self) -> Result<u64, Box<dyn Error>> {
        // No API in KVMi at the moment
        // fake 512MB
        let max_addr = 1024 * 1024 * 512;
        Ok(max_addr)
    }

    fn read_registers(&self, vcpu: u16) -> Result<Registers, Box<dyn Error>> {
        let (regs, sregs, _msrs) = self.kvmi.get_registers(vcpu)?;
        // TODO: hardcoded for x86 for now
        Ok(Registers::X86(X86Registers {
            rax: regs.rax,
            rbx: regs.rbx,
            rcx: regs.rcx,
            rdx: regs.rdx,
            rsi: regs.rsi,
            rdi: regs.rdi,
            rsp: regs.rsp,
            rbp: regs.rbp,
            r8: regs.r8,
            r9: regs.r9,
            r10: regs.r10,
            r11: regs.r11,
            r12: regs.r12,
            r13: regs.r13,
            r14: regs.r14,
            r15: regs.r15,
            rip: regs.rip,
            rflags: regs.rflags,
            cr0: sregs.cr0,
            cr3: sregs.cr3,
            cr4: sregs.cr4,
            fs_base: sregs.fs.base,
        }))
    }

    fn pause(&mut self) -> Result<(), Box<dyn Error>> {
        debug!("pause");
        // already paused ?
        if self.expect_pause_ev > 0 {
            return Ok(());
        }

        self.kvmi.pause()?;
        self.expect_pause_ev = self.kvmi.get_vcpu_count()?;
        debug!("expected pause events: {}", self.expect_pause_ev);
        Ok(())
    }

    fn resume(&mut self) -> Result<(), Box<dyn Error>> {
        debug!("resume");
        // already resumed ?
        if self.expect_pause_ev == 0 {
            return Ok(());
        }

        while self.expect_pause_ev > 0 {
            // wait
            let kvmi_event = self.kvmi.wait_and_pop_event(1000)?.unwrap();
            match kvmi_event.ev_type {
                KVMiEventType::PauseVCPU => {
                    debug!("VCPU {} - Received Pause Event", kvmi_event.vcpu);
                    self.expect_pause_ev -= 1;
                    self.kvmi.reply(&kvmi_event, KVMiEventReply::Continue)?;
                }
                _ => panic!(
                    "Unexpected {:?} event type while resuming VM",
                    kvmi_event.ev_type
                ),
            }
        }
        Ok(())
    }

    fn toggle_intercept(
        &mut self,
        vcpu: u16,
        intercept_type: InterceptType,
        enabled: bool,
    ) -> Result<(), Box<dyn Error>> {
        match intercept_type {
            InterceptType::Cr(micro_cr_type) => {
                let kvmi_cr = match micro_cr_type {
                    CrType::Cr0 => KVMiCr::Cr0,
                    CrType::Cr3 => KVMiCr::Cr3,
                    CrType::Cr4 => KVMiCr::Cr4,
                };
                Ok(self.kvmi.control_cr(vcpu, kvmi_cr, enabled)?)
            }
        }
    }

    fn listen(&mut self, timeout: u32) -> Result<Option<Event>, Box<dyn Error>> {
        // wait for next event and pop it
        debug!("wait for next event");
        let kvmi_event_opt = self.kvmi.wait_and_pop_event(timeout.try_into().unwrap())?;
        match kvmi_event_opt {
            None => Ok(None),
            Some(kvmi_event) => {
                let microvmi_event_kind = match kvmi_event.ev_type {
                    KVMiEventType::Cr { cr_type, new, old } => EventType::Cr {
                        cr_type: match cr_type {
                            KVMiCr::Cr0 => CrType::Cr0,
                            KVMiCr::Cr3 => CrType::Cr3,
                            KVMiCr::Cr4 => CrType::Cr4,
                        },
                        new,
                        old,
                    },
                    KVMiEventType::PauseVCPU => panic!("Unexpected PauseVCPU event. It should have been popped by resume VM. (Did you forget to resume your VM ?)"),
                    _ => unimplemented!()
                };

                let vcpu = kvmi_event.vcpu;
                let vcpu_index: usize = vcpu.try_into().unwrap();
                self.vec_events[vcpu_index] = Some(kvmi_event);

                Ok(Some(Event {
                    vcpu,
                    kind: microvmi_event_kind,
                }))
            }
        }
    }

    fn reply_event(
        &mut self,
        event: Event,
        reply_type: EventReplyType,
    ) -> Result<(), Box<dyn Error>> {
        let kvm_reply_type = match reply_type {
            EventReplyType::Continue => KVMiEventReply::Continue,
        };
        // get KVMiEvent associated with this VCPU
        let vcpu_index: usize = event.vcpu.try_into().unwrap();
        let kvmi_event = mem::replace(&mut self.vec_events[vcpu_index], None).unwrap();
        Ok(self.kvmi.reply(&kvmi_event, kvm_reply_type)?)
    }
}

impl<T: KVMIntrospectable> Drop for Kvm<T> {
    fn drop(&mut self) {
        debug!("KVM driver close");
        // disable all control register interception
        for vcpu in 0..self.get_vcpu_count().unwrap() {
            self.kvmi
                .control_events(vcpu, KVMiInterceptType::Cr, false)
                .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvmi::{kvm_msrs, kvm_regs, kvm_sregs};
    use mockall::mock;
    use mockall::predicate::{eq, function};
    use std::fmt::{Debug, Formatter};
    use test_case::test_case;

    #[test]
    fn test_fail_to_create_kvm_driver_if_kvmi_init_returns_error() {
        let mut kvmi_mock = MockKVMi::default();
        kvmi_mock.expect_init().returning(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "something went wrong",
            ))
        });

        let result = Kvm::new("some_vm", kvmi_mock);

        assert!(result.is_err(), "Expected error, got ok instead!");
    }

    #[test_case(1; "single vcpu")]
    #[test_case(2; "two vcpus")]
    #[test_case(16; "sixteen vcpus")]
    fn test_create_kvm_driver_if_guest_domain_is_valid(vcpu_count: u32) {
        let mut kvmi_mock = MockKVMi::default();
        kvmi_mock.expect_init().returning(|_| Ok(()));
        kvmi_mock
            .expect_get_vcpu_count()
            .returning(move || Ok(vcpu_count));
        for vcpu in 0..vcpu_count {
            kvmi_mock
                .expect_control_events()
                .with(
                    eq(vcpu as u16),
                    function(|x| matches!(x, KVMiInterceptType::Cr)),
                    eq(true),
                )
                .times(1)
                .returning(|_, _, _| Ok(()));
            kvmi_mock
                .expect_control_events()
                .with(
                    eq(vcpu as u16),
                    function(|x| matches!(x, KVMiInterceptType::Cr)),
                    eq(false),
                )
                .times(1)
                .returning(|_, _, _| Ok(()));
        }

        let result = Kvm::new("some_vm", kvmi_mock);

        assert!(result.is_ok(), "Expected ok, got error instead!");
    }

    mock! {
        KVMi{}
        trait Debug {
            fn fmt<'a>(&self, f: &mut Formatter<'a>) -> std::fmt::Result;
        }
        trait KVMIntrospectable: Debug {
            fn init(&mut self, socket_path: &str) -> Result<(), std::io::Error>;
            fn control_events(
                &self,
                vcpu: u16,
                intercept_type: KVMiInterceptType,
                enabled: bool,
            ) -> Result<(), std::io::Error>;
            fn control_cr(&self, vcpu: u16, reg: KVMiCr, enabled: bool) -> Result<(), std::io::Error>;
            fn control_msr(&self, vcpu: u16, reg: u32, enabled: bool) -> Result<(), std::io::Error>;
            fn read_physical(&self, gpa: u64, buffer: &mut [u8]) -> Result<(), std::io::Error>;
            fn write_physical(&self, gpa: u64, buffer: &[u8]) -> Result<(), std::io::Error>;
            fn get_page_access(&self, gpa: u64) -> Result<u8, std::io::Error>;
            fn set_page_access(&self, gpa: u64, access: u8) -> Result<(), std::io::Error>;
            fn pause(&self) -> Result<(), std::io::Error>;
            fn get_vcpu_count(&self) -> Result<u32, std::io::Error>;
            fn get_registers(&self, vcpu: u16) -> Result<(kvm_regs, kvm_sregs, kvm_msrs), std::io::Error>;
            fn set_registers(&self, vcpu: u16, regs: &kvm_regs) -> Result<(), std::io::Error>;
            fn wait_and_pop_event(&self, ms: i32) -> Result<Option<KVMiEvent>, std::io::Error>;
            fn reply(&self, event: &KVMiEvent, reply_type: KVMiEventReply) -> Result<(), std::io::Error>;
            fn get_maximum_gfn(&self) -> Result<u64, std::io::Error>;
        }
    }
}
