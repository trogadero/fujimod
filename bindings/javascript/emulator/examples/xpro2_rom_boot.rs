//! Execute a declared sibling Agate ROM against virtual SD files, without DRAM preload.
use fujimod_emulator_cpu::{ExecutionPolicy, Register};
use fujimod_emulator_javascript::Session;
use serde_json::json;
use std::path::PathBuf;

struct Handoff {
    cpu: std::rc::Rc<dyn fujimod_emulator_cpu::Cpu>,
    sha256: Option<String>,
    error: Option<String>,
}
unsafe fn handoff(_:u64, _:u32, data:*mut std::ffi::c_void) {
    let observed=unsafe { &mut *data.cast::<Handoff>() };
    if observed.sha256.is_some() || observed.error.is_some() {return;}
    let mut image=vec![0;40235008];
    match observed.cpu.read_memory(0,&mut image) {
        Ok(()) => observed.sha256=Some(fujimod_firmware::sha256(&image)),
        Err(error) => observed.error=Some(error.to_string()),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 6 { return Err("ROOT GRAPH INPUT OUTPUT_PREFIX TICKS".into()); }
    let root = PathBuf::from(&args[1]);
    let prefix = root.join(&args[4]);
    if !prefix.starts_with(root.join("analysis")) || prefix.components().any(|p| matches!(p, std::path::Component::ParentDir)) {
        return Err("output must be under analysis".into());
    }
    let mut handoffs: Vec<Box<Handoff>> = Vec::new();
    let mut s = Session::create_camera(&std::fs::read(root.join(&args[2]))?,
        &std::fs::read(root.join(&args[3]))?, ExecutionPolicy::Exact)?;
    let files = [("/FFSYS.BIN", 2048), ("/FFBOOT.BIN", 40235008)].into_iter().map(|(path, size)| {
        match s.core.card_read("sd", path, None, size) {
            Ok(bytes) => json!({"path":path,"bytes":bytes.len(),"sha256":fujimod_firmware::sha256(&bytes)}),
            Err(error) => json!({"path":path,"error":error}),
        }
    }).collect::<Vec<_>>();
    let mut initial = [0;64];
    s.core.board.read_physical(0x40000000, &mut initial)?;
    if initial.iter().any(|b| *b != 0) { return Err("DRAM is not initially empty".into()); }
    let boot_source=std::env::var("XPRO2_ROM_BOOT_SOURCE").unwrap_or("sd".into());
    if !matches!(boot_source.as_str(),"sd"|"nor") {return Err("boot source must be sd or nor".into());}
    s.core.logic_input("lab.sd_boot_strap", boot_source=="sd")?;
    let mut observed_pcs=vec![0,0x100,0x103000,0xfff00020,0xfff01888,0xfff019d4,0xfff01c04,0xfff027b8,0xfff02880];
    if let Ok(extra)=std::env::var("XPRO2_ROM_WATCH_PCS") {
        if extra.split(',').count()>16 {return Err("too many observed PCs".into());}
        for item in extra.split(',') {observed_pcs.push(u64::from_str_radix(item.trim_start_matches("0x"),16)?);}
    }
    observed_pcs.sort_unstable();observed_pcs.dedup();
    s.core.board.watch_pcs(&observed_pcs)?;
    handoffs.push(Box::new(Handoff{cpu:s.core.board.cpu_handle(0)?,sha256:None,error:None}));
    let hook=unsafe { s.core.board.cpu(0)?.install_hook(fujimod_emulator_cpu::Hook::Instruction {
        callback:handoff, data:(&mut **handoffs.last_mut().unwrap() as *mut Handoff).cast(),
        range:fujimod_emulator_cpu::HookRange::Inclusive{begin:0,end:0},
    })? };
    let limit = args[5].parse::<u64>()?;
    if limit == 0 || limit > 2_000_000_000 { return Err("tick budget outside1..2G".into()); }
    let mut failure = None;
    while s.core.board.now() < limit {
        let until = s.core.board.now().saturating_add(12_000_000).min(limit);
        if let Err(error) = s.run(until) { failure=Some(error); break; }
        eprintln!("ROM probe tick {}", s.core.board.now());
    }
    s.core.board.cpu(0)?.remove_hook(hook)?;
    let cores = (0..2).map(|core| {
        let registers = (0..16).map(|r| s.core.board.read_register(core,
            Register::Arm32(fujimod_emulator_cpu::arm::Register::R(r)))).collect::<Vec<_>>();
        let fault=(|| -> Result<_,String> {
            let cpu=s.core.board.cpu(core)?;
            let cp15=|crn,opc2|cpu.coprocessor(fujimod_emulator_cpu::arm::Coprocessor {
                cp:15,opc1:0,crn,crm:0,opc2,secure:0,wide:false},None);
            Ok(json!({"dfsr":cp15(5,0)?,"ifsr":cp15(5,1)?,"dfar":cp15(6,0)?,"ifar":cp15(6,2)?}))
        })();
        json!({"core":core,"registers":registers,"fault":fault})
    }).collect::<Vec<_>>();
    let frame = if let Some(frame) = s.frame(0)? {
        let path=prefix.with_extension("rgba");
        std::fs::write(&path,&frame.rgba)?;
        Some(json!({"path":path,"width":frame.width,"height":frame.height,"sha256":fujimod_firmware::sha256(&frame.rgba)}))
    } else {None};
    let state = s.core.board.state();
    let device_path = prefix.with_extension("devices.cbor");
    let mut device_file = std::fs::File::create(&device_path)?;
    ciborium::into_writer(&state["bus"]["devices"],&mut device_file)?;
    let snapshot=if failure.is_none() && handoffs[0].sha256.is_some() {
        let bytes=s.export_snapshot()?.snapshot;
        let path=prefix.with_extension("snapshot.bin");
        std::fs::write(&path,&bytes)?;
        Some(json!({"path":path,"sha256":fujimod_firmware::sha256(&bytes)}))
    }else{None};
    let report = json!({"boot_source":boot_source,"error":failure,"tick":s.core.board.now(),"cores":cores,
        "initial_dram_zero":true,"files":files,"frame":frame,"device_state":device_path,
        "handoff_image_sha256":handoffs[0].sha256,"handoff_read_error":handoffs[0].error,"snapshot":snapshot,
        "instructions":s.core.board.instruction_counts(),"pc_entries":s.core.board.pc_entries(),
        "limitations":["X-T2 reference ROM, not identified as the X-Pro2 mask ROM","Laboratory SD strap and A/B input"]});
    std::fs::write(prefix.with_extension("json"),serde_json::to_vec_pretty(&report)?)?;
    if let Some(error)=failure { return Err(error.into()); }
    Ok(())
}
