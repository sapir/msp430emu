use gdbstub::{Access, AccessKind, GdbStub, Target, TargetState};
use goblin::elf::Elf;
use microcorruption_emu::{
    disasm::AccessSize,
    emu::{EmulatorOpKind, Memory},
    Emulator, Error, REG_SR,
};
use std::{convert::TryInto, net::TcpListener, ops::Range};
use structopt::StructOpt;

pub const CPUOFF: u16 = 0x10;

fn convert_access_kind(op_kind: EmulatorOpKind) -> Option<AccessKind> {
    match op_kind {
        EmulatorOpKind::ReadReg | EmulatorOpKind::WriteReg => None,
        EmulatorOpKind::ReadMem => Some(AccessKind::Read),
        EmulatorOpKind::WriteMem => Some(AccessKind::Write),
    }
}

struct GdbEmulator(Emulator);

impl Target for GdbEmulator {
    type Usize = u16;
    type Error = Error;

    fn step(
        &mut self,
        mut log_mem_access: impl FnMut(Access<Self::Usize>),
    ) -> Result<TargetState, Self::Error> {
        self.0.step()?;

        for op in &self.0.last_ops {
            let kind = match convert_access_kind(op.kind) {
                Some(x) => x,
                None => continue,
            };

            let addr = op.addr;
            let value = op.value;

            log_mem_access(Access {
                kind,
                addr,
                val: value as u8,
            });

            if op.size == AccessSize::Word {
                log_mem_access(Access {
                    kind: convert_access_kind(op.kind).unwrap(),
                    addr: addr + 1,
                    val: (value >> 8) as u8,
                });
            }
        }

        let state = if (self.0.regs[REG_SR] & CPUOFF) != 0 {
            TargetState::Halted
        } else {
            TargetState::Running
        };

        Ok(state)
    }

    fn read_registers(&mut self, mut push_reg: impl FnMut(&[u8])) {
        for reg in 0..16 {
            push_reg(&self.0.regs[reg].to_le_bytes());
        }
    }

    fn write_registers(&mut self, regs: &[u8]) {
        for (i, value) in regs.chunks_exact(2).enumerate() {
            let value = u16::from_le_bytes(value.try_into().unwrap());
            self.0.regs[i.try_into().unwrap()] = value;
        }
    }

    fn read_pc(&mut self) -> Self::Usize {
        self.0.pc()
    }

    fn read_addrs(&mut self, addr: Range<Self::Usize>, mut val: impl FnMut(u8)) {
        for addr0 in addr {
            val(self.0.mem.get_byte(addr0))
        }
    }

    fn write_addrs(&mut self, mut get_addr_val: impl FnMut() -> Option<(Self::Usize, u8)>) {
        while let Some((addr, val)) = get_addr_val() {
            self.0.mem.set_byte(addr, val);
        }
    }

    fn target_description_xml() -> Option<&'static str> {
        Some(
            r#"
<target version="1.0">
    <architecture>msp430</architecture>
</target>"#,
        )
    }
}

fn load_dump<P: AsRef<std::path::Path>>(path: P) -> goblin::error::Result<Emulator> {
    let buf = std::fs::read(path)?;
    let elf = Elf::parse(&buf);

    if let Err(goblin::error::Error::BadMagic(_)) = elf {
        if buf.len() == 0x1_0000 {
            eprintln!(concat!(
                "Dump file is not a valid ELF, but it's 64KB long,",
                " assuming it's a raw memory dump"
            ));

            let mem = Memory::from_buf(buf);
            let emu = Emulator::from_initial_memory(mem);
            return Ok(emu);
        }
    }

    let elf = elf?;

    // Load program headers
    let mem = {
        let mut mem = Memory::new();

        for phdr in &elf.program_headers {
            assert_eq!(phdr.p_filesz, phdr.p_memsz);

            let segment = &buf[phdr.file_range()];
            mem.data.copy_from_slice(segment);
        }

        mem
    };

    Ok(Emulator::from_initial_memory(mem))
}

/// Run a gdbserver that emulates the microcorruption CTF device
#[derive(Debug, StructOpt)]
struct CliOpts {
    /// Path to an ELF file or raw memory dump
    dump: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = CliOpts::from_args();

    let emu = load_dump(&opts.dump)?;
    let mut emu = GdbEmulator(emu);

    let sockaddr = format!("localhost:{}", 9001);
    eprintln!("Waiting for a GDB connection on {:?}...", sockaddr);
    let sock = TcpListener::bind(sockaddr)?;
    let (stream, addr) = sock.accept()?;
    eprintln!("Debugger connected from {}", addr);

    // Hand the connection off to the GdbStub.
    let mut debugger = GdbStub::new(stream);

    let system_result = match debugger.run(&mut emu) {
        Ok(state) => {
            eprintln!("Disconnected from GDB. Target state: {:?}", state);
            Ok(())
        }
        Err(gdbstub::Error::TargetError(e)) => Err(e),
        Err(e) => return Err(e.into()),
    };

    eprintln!("{:?}", system_result);

    Ok(())
}