//! Retain a validated warm machine so input/display probes do not repeat boot.
use fujimod_emulator_cpu::ExecutionPolicy;
use fujimod_emulator_javascript::Session;
use serde_json::json;
use std::path::PathBuf;

struct UiWrites {
    core:usize,
    cpu:std::rc::Rc<dyn fujimod_emulator_cpu::Cpu>,
    events:Vec<serde_json::Value>,
    dropped:usize,
}
struct UiInstructions {
    core: usize,
    cpu: std::rc::Rc<dyn fujimod_emulator_cpu::Cpu>,
    codes: Vec<u32>,
    events: Vec<serde_json::Value>,
    dropped: usize,
}
unsafe fn ui_instruction(pc:u64, _:u32, data:*mut std::ffi::c_void) {
    let trace=unsafe { &mut *data.cast::<UiInstructions>() };
    // The Session owns this CPU throughout hook lifetime. Only fixed RAM and
    // registers are read; hooks are removed before either owner is dropped.
    let cpu=&*trace.cpu;
    let mut code=[0;4];
    if cpu.read_memory(0x109ce20,&mut code).is_err() { return; }
    let code=u32::from_le_bytes(code);
    // Graphics workers need not run under the input event's current UI code.
    if pc!=0x21d8734 && !trace.codes.is_empty() && !trace.codes.contains(&code) { return; }
    let registers:Vec<_>=(0..4).map(|i| {
        let mut raw=[0;4];
        cpu.read_register(fujimod_emulator_cpu::Register::Arm32(fujimod_emulator_cpu::arm::Register::R(i)),&mut raw)
            .ok().map(|_|u32::from_le_bytes(raw))
    }).collect();
    let draw_header=if pc==0x100b00 || pc==0x1fbafec {
        registers[0].and_then(|address| {
            let mut raw=[0;16];cpu.read_memory(u64::from(address),&mut raw).ok().map(|_|raw.chunks_exact(4).map(|v|u32::from_le_bytes(v.try_into().unwrap())).collect::<Vec<_>>())
        })
    }else{None};
    // For the text-entry diagnostic, retain one witness per resource ID so
    // repeated stock redraws cannot exhaust the trace before the added rows.
    if let Some(header)=&draw_header {
        if trace.events.iter().any(|event|event["drawHeader"][1].as_u64()==Some(u64::from(header[1]))) { return; }
    }
    if trace.events.len()>=256 { trace.dropped+=1; return; }
    let mut key_state=[0;2];
    let key_state=cpu.read_memory(0x1a61bcc,&mut key_state).ok().map(|_|u16::from_le_bytes(key_state));
    let mut rec_state=[0;2];
    let rec_state=cpu.read_memory(0x1a61bfc,&mut rec_state).ok().map(|_|u16::from_le_bytes(rec_state));
    let mut packet=[0;16];
    let packet=cpu.read_memory(0x109ce20,&mut packet).ok().map(|_|packet.chunks_exact(4)
        .map(|v|u32::from_le_bytes(v.try_into().unwrap())).collect::<Vec<_>>());
    let dispatcher_state=if pc==0x126e928 {
        registers[0].filter(|id|*id<256).and_then(|id| {
            let mut raw=[0;2];cpu.read_memory(0x1a61b40+u64::from(id)*2,&mut raw).ok().map(|_|u16::from_le_bytes(raw))
        })
    }else{None};
    let mut lr=[0;4];
    let lr=cpu.read_register(fujimod_emulator_cpu::Register::Arm32(fujimod_emulator_cpu::arm::Register::R(14)),&mut lr).ok().map(|_|u32::from_le_bytes(lr));
    // Original X-Pro2 menu getter a52058 reads one byte, not a word.
    // Record at the drawing call itself, before inferring any UI predicate.
    let byte=|address| {
        let mut raw=[0;1];cpu.read_memory(address,&mut raw).ok().map(|_|raw[0])
    };
    let attributes=match pc {
        0x103800 | 0xb5accc => registers[2],
        0xb5ea98 => registers[3],
        _ => None,
    };
    let layer_alpha=attributes.and_then(|address|byte(u64::from(address)+10));
    // Bounded observation of the native graphics queue. The command format
    // differs from X70, so capture it before translating theme field offsets.
    // 112 bytes remain below the per-record binary JSON limit of 128 bytes.
    let command_header=if pc==0x21d8734 {
        registers[1].and_then(|address| {
            let mut raw=[0;112];
            cpu.read_memory(u64::from(address),&mut raw).ok().map(|_|raw.chunks_exact(4)
                .map(|v|u32::from_le_bytes(v.try_into().unwrap())).collect::<Vec<_>>())
        })
    }else{None};
    trace.events.push(json!({"core":trace.core,"pc":pc,"lr":lr,"uiCode":code,"uiPacket":packet,"registers":registers,"dispatcherState":dispatcher_state,"keyMachineState":key_state,"recMachineState":rec_state,
        "menuActive":byte(0x10041bc),"systemMode":byte(0x10a7fbc),"displaySelect":byte(0xf4caf7),"layerAlpha":layer_alpha,"drawHeader":draw_header,"commandHeader":command_header}));
}
unsafe fn ui_write(_:fujimod_emulator_cpu::Access, address:u64, size:i32, value:i64, data:*mut std::ffi::c_void) {
    // Each context belongs to one exact-mode CPU and outlives its hook.
    let trace=unsafe { &mut *(data.cast::<UiWrites>()) };
    if trace.events.len()<256 {
        let register=|register| {
            let mut raw=[0;4];
            trace.cpu.read_register(register,&mut raw).ok().map(|_|u32::from_le_bytes(raw))
        };
        let mut packet=[0;16];
        let packet=trace.cpu.read_memory(0x109ce20,&mut packet).ok().map(|_|packet.chunks_exact(4).map(|v|u32::from_le_bytes(v.try_into().unwrap())).collect::<Vec<_>>());
        trace.events.push(json!({"core":trace.core,"address":address,"size":size,"value":value,"uiPacket":packet,
            "pc":register(fujimod_emulator_cpu::Register::Pc),
            "lr":register(fujimod_emulator_cpu::Register::Arm32(fujimod_emulator_cpu::arm::Register::R(14)))}));
    }
    else { trace.dropped+=1; }
}

fn advance(s: &mut Session, ticks: u64) -> Result<(), String> {
    let result=s.run(
        s.core
            .board
            .now()
            .checked_add(ticks)
            .ok_or("tick overflow")?,
    );
    if let Err(error)=&result {
        let mut cores=Vec::new();
        for core in 0..2 {
            if let Ok(cpu)=s.core.board.cpu(core) {
                let registers:Vec<_>=(0..16).map(|i| {
                    let mut raw=[0;4];cpu.read_register(fujimod_emulator_cpu::Register::Arm32(
                        fujimod_emulator_cpu::arm::Register::R(i)),&mut raw).ok().map(|_|u32::from_le_bytes(raw))
                }).collect();
                cores.push(json!({"core":core,"registers_r0_to_pc":registers}));
            }
        }
        eprintln!("Advance failed {}",json!({"reason":error,"tick":s.core.board.now(),"cores":cores}));
    }
    result
}
fn write_new_snapshot(path:&std::path::Path,bytes:&[u8])->std::io::Result<()> {
    use std::io::{Seek,SeekFrom,Write};
    let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    for chunk in bytes.chunks(4096) {
        if chunk.iter().all(|byte|*byte==0) { file.seek(SeekFrom::Current(chunk.len() as i64))?; }
        else { file.write_all(chunk)?; }
    }
    file.set_len(bytes.len() as u64)
}
fn preferences_observation(s:&Session)->Result<serde_json::Value,String> {
    let mut state=[0;68];s.core.board.read_physical(0x102400,&mut state)?;
    let mut opacity=[0;16];s.core.board.read_physical(0x103b00,&mut opacity)?;
    let word=|offset|u32::from_le_bytes(state[offset..offset+4].try_into().unwrap());
    let theme=if std::env::var("XPRO2_THEME_OBSERVE").as_deref()==Ok("1") {
        let mut raw=[0;4];s.core.board.read_physical(0x90700,&mut raw)?;
        Some(u32::from_le_bytes(raw))
    }else{None};
    let guide=if std::env::var("XPRO2_GUIDE_OBSERVE").as_deref()==Ok("1") {
        let mut raw=[0;4];s.core.board.read_physical(0xa9000,&mut raw)?;
        Some(u32::from_le_bytes(raw))
    }else{None};
    Ok(json!({"phase":word(8),"error":word(44),"loads":word(48),"saves":word(52),"lock":word(64),
        "opacity":opacity.chunks_exact(4).map(|v|u32::from_le_bytes(v.try_into().unwrap())).collect::<Vec<_>>(),"theme":theme,"guide":guide}))
}
fn tap(s: &mut Session, id: &str) -> Result<(), String> {
    s.press(id.into(), None)?;
    advance(s, 4_800_000)?;
    s.release(id.into())?;
    advance(s, 4_800_000)
}
fn osd(s:&Session, target:&std::path::Path, phase:&str) -> Result<serde_json::Value,Box<dyn std::error::Error>> {
    let state=s.core.board.state();
    let output=state["bus"]["devices"].as_array().ok_or("devices")?.iter()
        .find(|node|node["kind"]=="xpp-display-output").ok_or("display")?;
    let words=output["state"]["words"].as_array().ok_or("display words")?;
    let word=|offset:u64|words.iter().find(|v|v[0].as_u64()==Some(offset)).and_then(|v|v[1].as_u64()).unwrap_or(0);
    let size=word(0x220);let width=((size>>16)&0xfff)+1;let height=(size&0xfff)+1;
    let address=word(0x230);
    if address==0 || width*height>4_000_000{return Ok(json!({"available":false}));}
    let mut bytes=vec![0;(width*height*4) as usize];s.core.board.read_physical(address,&mut bytes)?;
    let path=if std::env::var("XPRO2_DUMP_OSD").as_deref()==Ok("1") {
        let path=target.with_extension(format!("{}-{}.rgba",std::process::id(),phase));
        std::fs::write(&path,&bytes)?;Some(path)
    }else{None};
    Ok(json!({"address":address,"width":width,"height":height,"sha256":fujimod_firmware::sha256(&bytes),"path":path}))
}

fn apply_scene_input(session: &mut Session, root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(path) = std::env::var("XPRO2_SCENE_RGB") {
        let width = std::env::var("XPRO2_SCENE_WIDTH").unwrap_or("960".into()).parse()?;
        let height = std::env::var("XPRO2_SCENE_HEIGHT").unwrap_or("640".into()).parse()?;
        session.set_scene(Some((width, height, std::fs::read(root.join(path))?)))?;
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 4 {
        return Err("ROOT GRAPH SNAPSHOT [CONTROL]".into());
    }
    let root = PathBuf::from(&args[1]);
    let graph = root.join(&args[2]);
    let firmware = root.join(std::env::var("XPRO2_FIRMWARE").unwrap_or("FWUP0008_511.DAT".into()));
    if !firmware.canonicalize()?.starts_with(root.canonicalize()?) {
        return Err("Firmware input must remain inside the workspace".into());
    }
    let target = root.join(&args[3]);
    let reused = target.exists();
    let stage=std::env::var("XPRO2_CHECKPOINT_STAGE").unwrap_or("rec".into());
    if stage!="date" && stage!="rec" && stage!="rec-fast" { return Err("checkpoint stage must be date, rec or rec-fast".into()); }
    if stage=="rec-fast" && !target.exists() { return Err("rec-fast requires the existing diagnostic snapshot".into()); }
    // Declared before session: contexts also outlive CPU teardown on errors.
    let mut ui_traces:Vec<Box<UiWrites>>=Vec::new();
    let mut instruction_traces:Vec<Box<UiInstructions>>=Vec::new();
    let mut ui_hooks=Vec::new();
    let mut session = if reused {
        Session::load_snapshot(&std::fs::read(&target)?)?
    } else {
        let mut s = Session::create_camera(
            &std::fs::read(&graph)?,
            &std::fs::read(&firmware)?,
            ExecutionPolicy::Exact,
        )?;
        if std::env::var("XPRO2_ROM_SD_BOOT").as_deref()==Ok("1") {
            s.core.logic_input("lab.sd_boot_strap",true)?;
        }
        apply_scene_input(&mut s, &root)?;
        advance(&mut s, 240_000_000)?;
        eprintln!("Initial menu at tick {}", s.core.board.now());
        tap(&mut s, "menu_ok_button")?;
        advance(&mut s, 96_000_000)?;
        if stage=="rec" {
            tap(&mut s, "disp_back_button")?;
            advance(&mut s, 12_000_000)?;
        }
        if let Ok(ticks)=std::env::var("XPRO2_PRE_SNAPSHOT_IDLE_TICKS") {
            let ticks=ticks.parse::<u64>()?;
            if ticks>480_000_000 { return Err("pre-snapshot idle exceeds ten simulated seconds".into()); }
            advance(&mut s,ticks)?;
        }
        let before = serde_json::to_value(s.fingerprint()?)?;
        let snapshot = s.export_snapshot()?.snapshot;
        let restored = Session::load_snapshot(&snapshot)?;
        if serde_json::to_value(restored.fingerprint()?)? != before {
            return Err("Warm snapshot roundtrip fingerprint differs".into());
        }
        // A new path is required: never replace an earlier captured machine.
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_new_snapshot(&target,&snapshot)?;
        eprintln!("Warm snapshot validated, {} bytes", snapshot.len());
        restored
    };
    let expected: serde_json::Value = serde_json::from_slice(&std::fs::read(&graph)?)?;
    if session.core.definition != expected {
        return Err("Snapshot graph differs; use a new snapshot path for changed inputs".into());
    }
    if expected["graph"]["firmware"]["sha256"].as_str()
        != Some(&fujimod_firmware::sha256(&std::fs::read(&firmware)?)) {
        return Err("Snapshot graph and current firmware identity differ".into());
    }
    if reused {
        if std::env::var("XPRO2_ROM_SD_BOOT").as_deref()==Ok("1") {
            session.core.logic_input("lab.sd_boot_strap",true)?;
        }
        apply_scene_input(&mut session, &root)?;
    }
    if let Ok(contacts)=std::env::var("XPRO2_SD_CONTACTS") {
        let (present,protected)=match contacts.as_str() {
            "present"=>(true,false),"protected"=>(true,true),"absent"=>(false,false),
            _=>return Err("SD contacts must be present, protected or absent".into()),
        };
        session.core.card_contacts("sd",present,protected)?;
    }
    // Declared external voltages, applied through the ADC capability, never RAM patches.
    if let Ok(values)=std::env::var("XPRO2_ADC_SAMPLES") {
        let samples=values.split(',').map(str::parse::<u16>).collect::<Result<Vec<_>,_>>()?;
        if samples.len()!=12 || samples.iter().any(|v|*v>1023) { return Err("expected twelve 10-bit ADC samples".into()); }
        session.core.set_adc_samples("adc",&samples)?;
    }
    let memory_points=std::env::var("XPRO2_TRACE_RAM").ok().map(|values| {
        if values.split(',').count()>16 { return Err("too many RAM watches".to_string()); }
        values.split(',').map(|value| {
            let address=u64::from_str_radix(value.trim().trim_start_matches("0x"),16).map_err(|e|e.to_string())?;
            if address>0x1fff_fffc || address&3!=0 { return Err("RAM watch must be aligned".into()); }
            Ok((address,4_u64))
        }).collect::<Result<Vec<_>,String>>()
    }).transpose()?;
    let memory_access=std::env::var("XPRO2_TRACE_RAM_ACCESS").unwrap_or("write".into());
    if !matches!(memory_access.as_str(),"read"|"write"|"both") { return Err("RAM trace access must be read, write or both".into()); }
    if std::env::var("XPRO2_TRACE_UI").as_deref()==Ok("1") || memory_points.is_some() {
        for core in 0..2 {
            ui_traces.push(Box::new(UiWrites{core,cpu:session.core.board.cpu_handle(core)?,events:Vec::new(),dropped:0}));
            let data=(&mut **ui_traces.last_mut().unwrap() as *mut UiWrites).cast();
            let defaults=[(0x109ce20_u64,4),(0x1b202ec,4),
                (0x1a61bcc,2),(0x1a61b40,2),(0x10a72ec,2),
                (0x10a7fbc,1),(0x10aca78,4),(0xff395c,1),
                (0x10c3a2e,2),(0x10c3a30,2),(0x1010050,4),
                (0x101945c,1),(0x10bd0a4,1),(0x10bd0a8,4)];
            for &(address,size) in memory_points.as_deref().unwrap_or(&defaults) {
                let hook=unsafe { session.core.board.cpu(core)?.install_hook(fujimod_emulator_cpu::Hook::Memory {
                    callback:ui_write,data,read:memory_access!="write",write:memory_access!="read",
                    range:fujimod_emulator_cpu::HookRange::Inclusive{begin:address,end:address+size-1},
                })? };
                ui_hooks.push((core,hook));
            }
        }
    }
    let osd_before=osd(&session,&target,"before")?;
    let mut row_probe = Vec::new();
    let mut control_runs=Vec::new();
    if let Some(control) = args.get(4) {
        let mut watched = vec![
            0xb46e5c, 0xb47224, 0xb47918, 0xb47ca4, 0xb47d24, 0xb47d2c, 0xb47de8, 0xb48800, 0xb49798, 0xb49824, 0xb4a89c, 0xb4aa80,
            0xb4ba1c, 0xb58aa4, 0xba9d2c, 0xba9dcc, 0xbaa0bc, 0x1362e84, 0x144bc0c, 0x144bc1c, 0x144bc34,
        ];
        // Bounded, read-only instruction witnesses for follow-up investigations.
        // Addresses are ARM instruction PCs in hexadecimal, comma separated.
        let mut trace_points=Vec::new();
        if let Ok(extra) = std::env::var("XPRO2_WATCH_PCS") {
            if extra.split(',').count() > 128 { return Err("too many watched PCs".into()); }
            for value in extra.split(',').filter(|value| !value.is_empty()) {
                let pc = u64::from_str_radix(value.trim().trim_start_matches("0x"), 16)?;
                if pc & 3 != 0 { return Err("watched ARM PC is not aligned".into()); }
                watched.push(pc);
                trace_points.push(pc);
            }
        }
        if std::env::var("XPRO2_COLD_CYCLE").as_deref()==Ok("1") {
            if !trace_points.is_empty() || std::env::var("XPRO2_TRACE_PCS").as_deref()==Ok("1") {
                return Err("cold cycle requires the constructor observation wiring; run added PC traces separately".into());
            }
            watched.clear();
        }
        if std::env::var("XPRO2_COLD_CYCLE").as_deref()==Ok("1") {
            if !trace_points.is_empty() || std::env::var("XPRO2_TRACE_PCS").as_deref()==Ok("1") {
                return Err("cold-cycle proof requires unchanged PC observation wiring".into());
            }
            // Reset restores the checked constructor topology. Optional lab
            // PC counters added after construction must not alter that wiring.
            watched.clear();
        }
        watched.sort_unstable();
        watched.dedup();
        let existing:std::collections::BTreeSet<_>=session.core.board.observation_topology()
            .into_iter().flat_map(|(_,points)|points).collect();
        let additional:Vec<_>=watched.iter().copied().filter(|pc|!existing.contains(pc)).collect();
        session.core.board.clear_pc_entries();
        session.core.board.watch_pcs(&additional)?;
        if std::env::var("XPRO2_TRACE_PCS").as_deref()==Ok("1") {
            let codes=std::env::var("XPRO2_TRACE_EVENTS").unwrap_or("1,801".into());
            if codes.split(',').count()>32 { return Err("too many traced UI event codes".into()); }
            let codes=if codes=="*" { Vec::new() } else { codes.split(',').map(|code|u32::from_str_radix(code.trim().trim_start_matches("0x"),16))
                .collect::<Result<Vec<_>,_>>()? };
            for core in 0..2 {
                let cpu=session.core.board.cpu_handle(core)?;
                instruction_traces.push(Box::new(UiInstructions{core,cpu,codes:codes.clone(),events:Vec::new(),dropped:0}));
                let data=(&mut **instruction_traces.last_mut().unwrap() as *mut UiInstructions).cast();
                for &pc in if trace_points.is_empty() { &watched } else { &trace_points } {
                    let hook=unsafe { session.core.board.cpu(core)?.install_hook(fujimod_emulator_cpu::Hook::Instruction {
                        callback:ui_instruction,data,
                        range:fujimod_emulator_cpu::HookRange::Inclusive{begin:pc,end:pc},
                    })? };
                    ui_hooks.push((core,hook));
                }
            }
        }
        for phase in ["before", "pressed", "released"] {
            if phase == "pressed" {
                session.press(control.clone(), None)?;
                advance(&mut session, 4_800_000)?;
            }
            if phase == "released" {
                session.release(control.clone())?;
                advance(&mut session, 4_800_000)?;
            }
            let mut row = [0; 28];
            session.core.board.read_physical(0xe29dfc, &mut row)?;
            row_probe.push(json!({"phase":phase,"menuRow":row}));
        }
        let settle=std::env::var("XPRO2_SETTLE_TICKS").unwrap_or("12000000".into()).parse::<u64>()?;
        if settle>480_000_000{return Err("settle exceeds ten simulated seconds".into());}
        advance(&mut session, settle)?;
        if let Ok(followup)=std::env::var("XPRO2_FOLLOWUP_CONTROL") {
            tap(&mut session,&followup)?;
            advance(&mut session,12_000_000)?;
        }
        if let Ok(controls)=std::env::var("XPRO2_FOLLOWUP_CONTROLS") {
            let controls:Vec<_>=controls.split(',').collect();
            if controls.is_empty() || controls.len()>32 { return Err("invalid followup control count".into()); }
            let settle=std::env::var("XPRO2_FOLLOWUP_SETTLE_TICKS").unwrap_or("96000000".into()).parse::<u64>()?;
            if !(12_000_000..=96_000_000).contains(&settle) { return Err("invalid followup settle".into()); }
            let settle_steps=std::env::var("XPRO2_FOLLOWUP_SETTLE_STEPS").ok().map(|values|
                values.split(',').map(str::parse::<u64>).collect::<Result<Vec<_>,_>>()
            ).transpose()?;
            if let Some(steps)=&settle_steps {
                if steps.len()!=controls.len() || steps.iter().any(|v|!(12_000_000..=96_000_000).contains(v)) {
                    return Err("per-step settle must match controls and remain12M..96M".into());
                }
            }
            let capture_steps=std::env::var("XPRO2_CAPTURE_MENU_STEPS").unwrap_or_default().split(',')
                .filter(|value|!value.is_empty()).map(str::parse::<usize>).collect::<Result<std::collections::BTreeSet<_>,_>>()?;
            if capture_steps.iter().any(|step|*step==0 || *step>32) {return Err("captured menu step must be1..32".into());}
            for (step,control) in controls.into_iter().enumerate() {
                if control.is_empty() || !control.bytes().all(|c|c.is_ascii_alphanumeric() || c==b'.' || c==b'_') {
                    return Err("invalid physical followup control".into());
                }
                eprintln!("Followup control {} at tick {}",control,session.core.board.now());
                tap(&mut session,control)?;
                advance(&mut session,settle_steps.as_ref().map_or(settle,|steps|steps[step]))?;
                let mut raw=[0;32];session.core.board.read_physical(0x1003980,&mut raw)?;
                let words:Vec<_>=raw.chunks_exact(4).map(|b|u32::from_le_bytes(b.try_into().unwrap())).collect();
                let descriptor=words[0];let mut property=[0;2];
                let property=if descriptor<0x1fff_fffc && descriptor!=0 {
                    session.core.board.read_physical(u64::from(descriptor)+2,&mut property)?;
                    Some(u16::from_le_bytes(property))
                }else{None};
                let mut opacity=[0;16];session.core.board.read_physical(0x103b00,&mut opacity)?;
                let opacity:Vec<_>=opacity.chunks_exact(4).map(|b|u32::from_le_bytes(b.try_into().unwrap())).collect();
                let preferences=if std::env::var("XPRO2_PREFS_OBSERVE").as_deref()==Ok("1") {Some(preferences_observation(&session)?)}else{None};
                let lcd=if capture_steps.contains(&(step+1)) {
                    if let Some(frame)=session.frame(0)? {
                        let path=target.with_extension(format!("{}-step-{}.rgba",std::process::id(),step+1));
                        std::fs::write(&path,&frame.rgba)?;
                        Some(json!({"path":path,"width":frame.width,"height":frame.height,"sha256":fujimod_firmware::sha256(&frame.rgba)}))
                    }else{None}
                }else{None};
                control_runs.push(json!({"control":control,"tick":session.core.board.now(),"menuState":words,
                    "property":property,"opacity":opacity,"preferences":preferences,"lcd":lcd,"osd":osd(&session,&target,"control")?}));
                eprintln!("Followup witness {}",control_runs.last().unwrap());
            }
        }
    }
    if let Ok(value)=std::env::var("XPRO2_IDLE_TICKS") {
        let ticks=value.parse::<u64>()?;
        if ticks>480_000_000 { return Err("idle exceeds ten simulated seconds".into()); }
        advance(&mut session,ticks)?;
    }
    // Explicit test input for raster qualification, separate from button/UI
    // evidence. No code or snapshot identity is changed. One warm machine can
    // render every guide, instead of booting the same image ten times.
    let mut guide_cases=Vec::new();
    if let Ok(sequence)=std::env::var("XPRO2_GUIDE_SEQUENCE") {
        let choices=sequence.split(',').map(str::parse::<u32>).collect::<Result<Vec<_>,_>>()?;
        if choices.is_empty() || choices.len()>16 || choices.iter().any(|value|*value>9) {
            return Err("guide sequence requires1..16 values in0..9".into());
        }
        let ticks=std::env::var("XPRO2_GUIDE_SETTLE_TICKS").unwrap_or("24000000".into()).parse::<u64>()?;
        if !(12_000_000..=96_000_000).contains(&ticks) {return Err("guide settle must remain12M..96M".into());}
        let mut signature=[0;4];session.core.board.read_physical(0xa9c00,&mut signature)?;
        let mut descriptor=[0;4];session.core.board.read_physical(0xa9020,&mut descriptor)?;
        let mut active=[0;1];session.core.board.read_physical(0x10041bc,&mut active)?;
        if u32::from_le_bytes(signature)!=0x58424d52 || u32::from_le_bytes(descriptor)!=0x01090048 || active[0]!=0 {
            return Err("guide sequence requires the framing module and a closed camera menu".into());
        }
        // Optional isolation of raster behavior from the camera's native
        // display visibility controls. Retain and restore the SDK test input;
        // reports label this intervention and it is never a UI qualification.
        let visibility_override=std::env::var("XPRO2_GUIDE_VISIBILITY_OVERRIDE").as_deref()==Ok("1");
        let mut visibility_before=[0;2];
        if visibility_override {
            session.core.board.read_physical(0x1d64654,&mut visibility_before)?;
            session.core.board.write_physical(0x1d64654,&[1,1])?;
        }
        let draw_mode_override=std::env::var("XPRO2_GUIDE_DRAW_MODE_OVERRIDE").as_deref()==Ok("7");
        let mut draw_override_address=None;
        if draw_mode_override {
            let mut code=[0;1536];session.core.board.read_physical(0xab400,&mut code)?;
            let pattern=[0x05,0x00,0xa0,0xe3,0x0c,0x00,0x8d,0xe5];
            let matches:Vec<_>=code.windows(8).enumerate().filter(|(offset,bytes)|offset%4==0 && *bytes==pattern).map(|(offset,_)|offset).collect();
            if matches.len()!=1 {return Err("mode7 isolation requires exactly one known mode5 instruction".into());}
            let address=0xab400+matches[0] as u64;
            session.core.board.write_physical(address,&0xe3a00007u32.to_le_bytes())?;
            draw_override_address=Some(address);
        }
        for (index,choice) in choices.into_iter().enumerate() {
            session.core.board.write_physical(0xa9000,&choice.to_le_bytes())?;
            advance(&mut session,ticks)?;
            let mut raw=[0;28];session.core.board.read_physical(0xa9000,&mut raw)?;
            let words:Vec<_>=raw.chunks_exact(4).map(|v|u32::from_le_bytes(v.try_into().unwrap())).collect();
            let lcd=if let Some(frame)=session.frame(0)? {
                let path=target.with_extension(format!("{}-lcd-guide-{index}-{choice}.rgba",std::process::id()));
                std::fs::write(&path,&frame.rgba)?;
                Some(json!({"path":path,"width":frame.width,"height":frame.height,"sha256":fujimod_firmware::sha256(&frame.rgba)}))
            }else{None};
            guide_cases.push(json!({"choice":choice,"selectionInjectedForRendering":true,"visibilityOverrideInjected":visibility_override,"drawMode7OverrideAddress":draw_override_address,
                "tick":session.core.board.now(),"stateWords":words,"lcd":lcd,
                "osd":osd(&session,&target,&format!("guide-{index}-{choice}"))?}));
            eprintln!("Guide witness {}",guide_cases.last().unwrap());
        }
        if visibility_override {session.core.board.write_physical(0x1d64654,&visibility_before)?;}
        if let Some(address)=draw_override_address {session.core.board.write_physical(address,&0xe3a00005u32.to_le_bytes())?;}
    }
    for (core,hook) in ui_hooks { session.core.board.cpu(core)?.remove_hook(hook)?; }
    let ui_writes:Vec<_>=ui_traces.iter().map(|trace|json!({"core":trace.core,"events":trace.events,"dropped":trace.dropped})).collect();
    let ui_instructions:Vec<_>=instruction_traces.iter().map(|trace|json!({"core":trace.core,"events":trace.events,"dropped":trace.dropped})).collect();
    let cold_cycle=if std::env::var("XPRO2_COLD_CYCLE").as_deref()==Ok("1") {
        let before=preferences_observation(&session)?;
        let pre_path=target.with_extension(format!("{}-before-cold.bin",std::process::id()));
        let pre_bytes=session.export_snapshot()?.snapshot;
        let pre_restored=Session::load_snapshot(&pre_bytes)?;
        if serde_json::to_value(pre_restored.fingerprint()?)?!=serde_json::to_value(session.fingerprint()?)? {
            return Err("pre-cycle snapshot roundtrip differs".into());
        }
        write_new_snapshot(&pre_path,&pre_bytes)?;
        eprintln!("Pre-cycle snapshot {} sha256 {}",pre_path.display(),fujimod_firmware::sha256(&pre_bytes));
        let before_path=pre_path.clone();
        let durable=session.save_durable()?.durable;
        let durable_before=fujimod_firmware::sha256(&durable);
        let fresh_constructor=std::env::var("XPRO2_COLD_FRESH_CONSTRUCTOR").as_deref()==Ok("1");
        if fresh_constructor {
            // UI diagnostics may add PC observers after the checked reset image.
            // Reconstruct the exact original graph/firmware, transferring only
            // identity-checked durable media and physical persistent positions.
            // Never transfer RAM or bypass keyframe topology validation.
            let mut fresh=Session::create_camera(
                &std::fs::read(&graph)?, &std::fs::read(&firmware)?, ExecutionPolicy::Exact)?;
            fresh.load_durable(&durable)?;
            session=fresh;
        } else {
            session.cycle_power(&[])?;
        }
        if std::env::var("XPRO2_ROM_SD_BOOT").as_deref()==Ok("1") {
            session.core.logic_input("lab.sd_boot_strap",true)?;
        }
        let fresh_counts=session.core.board.instruction_counts().to_vec();
        if fresh_counts.iter().any(|count|*count!=0) {return Err("cold cycle did not reset CPU instruction counts".into());}
        let fresh_cpu_pcs=(0..2).map(|core|session.core.board.read_register(core,fujimod_emulator_cpu::Register::Pc)).collect::<Vec<_>>();
        let fresh_rom_dram_zero=if std::env::var("XPRO2_ROM_SD_BOOT").as_deref()==Ok("1") {
            let mut bytes=[0;64];session.core.board.read_physical(0x40000000,&mut bytes)?;
            Some(bytes.iter().all(|b|*b==0))
        }else{None};
        let fresh=preferences_observation(&session)?;
        let durable_fresh=fujimod_firmware::sha256(&session.save_durable()?.durable);
        eprintln!("Cold reconstruction: counts {:?}, preferences {}",fresh_counts,fresh);
        let boot_ticks=if std::env::var("XPRO2_ROM_SD_BOOT").as_deref()==Ok("1") {360_000_000}else{240_000_000};
        advance(&mut session,boot_ticks)?;
        tap(&mut session,"menu_ok_button")?;
        advance(&mut session,96_000_000)?;
        tap(&mut session,"disp_back_button")?;
        advance(&mut session,108_000_000)?;
        Some(json!({"freshConstructor":fresh_constructor,"fresh_cpu_pcs":fresh_cpu_pcs,"fresh_rom_dram_zero":fresh_rom_dram_zero,"before":before,"before_snapshot":before_path,"fresh":fresh,"after":preferences_observation(&session)?,
            "fresh_cpu_counts":fresh_counts,"pre_cycle_snapshot":pre_path,"durable_before_sha256":durable_before,"durable_fresh_sha256":durable_fresh}))
    }else{None};
    let osd_after=osd(&session,&target,"after")?;
    let board_state = session.core.board.state();
    let devices_path=target.with_extension(format!("{}-devices.cbor",std::process::id()));
    let mut devices_bytes=Vec::new();ciborium::into_writer(&board_state["bus"]["devices"],&mut devices_bytes)?;
    std::fs::write(&devices_path,&devices_bytes)?;
    let devices_export=json!({"path":devices_path,"bytes":devices_bytes.len(),"sha256":fujimod_firmware::sha256(&devices_bytes)});
    let cp15=|crn,opc2|session.core.board.cpu(0)?.coprocessor(
        fujimod_emulator_cpu::arm::Coprocessor{cp:15,opc1:0,crn,crm:0,opc2,secure:0,wide:false},None);
    let arm_exception=json!({"sctlr":cp15(1,0)?,"vbar":cp15(12,0)?,"dfsr":cp15(5,0)?,"ifsr":cp15(5,1)?,"dfar":cp15(6,0)?,"ifar":cp15(6,2)?});
    let gpio_export=if let Some(device)=board_state["bus"]["devices"].as_array().and_then(|devices|devices.iter().find(|device|device["kind"]=="masked-gpio")) {
        let path=target.with_extension(format!("{}-gpio.cbor",std::process::id()));
        let mut bytes=Vec::new();ciborium::into_writer(&device["state"],&mut bytes)?;
        std::fs::write(&path,&bytes)?;
        Some(json!({"path":path,"bytes":bytes.len(),"sha256":fujimod_firmware::sha256(&bytes)}))
    } else { None };
    let lens_gpio=board_state["bus"]["devices"].as_array().ok_or("devices missing")?.iter()
        .find(|d|d["kind"]=="masked-gpio").map(|d|json!({"bank14":d["state"]["regs"][14],"input14":d["state"]["inputs"][14],
            "motorBank1":d["state"]["regs"][1],"shutterInputs":[d["state"]["inputs"][0],d["state"]["inputs"][10],d["state"]["inputs"][15]]}));
    // Inferred stock TCB layout, validated by exact known entry addresses.
    // This is a saved-context view, not an unwound call stack.
    let mut task_states=Vec::new();
    for address in (0x65650..0x70b70_u64).step_by(0x150) {
        let mut raw=[0;0x150];session.core.board.read_physical(address,&mut raw)?;
        let words:Vec<_>=raw.chunks_exact(4).map(|v|u32::from_le_bytes(v.try_into().unwrap())).collect();
        let name=match words[57] { 0xb40124=>"lensdata",0x129d294=>"ui",0x146ba08=>"reccont",
            0x15a0744=>"saclac",0x16cd068=>"ccdtg",0xc01510=>"fshut",0xbeae18=>"lensdc",
            0xb3b258=>"lenscom",0xb42160=>"lswatch",
            0x1696560=>"imgdc",
            0xb7c618=>"vglib",0xb625d8=>"graphic",0xc9a840=>"smaphp",0xc9a7cc=>"smaphk",_=>continue };
        let (sp,end)=(words[2] as u64,words[4] as u64);
        let stack=if sp>=0xf40000 && sp<=end && end<0x1000000 && end-sp<16384 {
            let mut bytes=vec![0;(end-sp+1) as usize];session.core.board.read_physical(sp,&mut bytes)?;
            let path=target.with_extension(format!("{}-{}-{}.stack.bin",std::process::id(),session.core.board.now(),name));
            std::fs::write(&path,&bytes)?;
            Some(json!({"path":path,"bytes":bytes.len(),"sha256":fujimod_firmware::sha256(&bytes)}))
        }else{None};
        task_states.push(json!({"name":name,"tcb":address,"entry":words[57],"savedSp":sp,
            "stackEnd":end,"stateRaw":words[62],"waitTypeRaw":words[67],"waitObjectRaw":words[69],"stack":stack}));
    }
    let raw_gic = &board_state["gic"];
    let gic_file = target.with_extension(format!("{}-{}.gic.cbor",std::process::id(),session.core.board.now()));
    let mut gic_bytes=Vec::new();
    ciborium::into_writer(raw_gic,&mut gic_bytes)?;
    std::fs::write(&gic_file,&gic_bytes)?;
    let ids = |name:&str| raw_gic[name].as_array().map(|cores|cores.iter().map(|core|
        core.as_array().map(|bits|bits.iter().enumerate().filter_map(|(id,bit)|
            (bit.as_bool()==Some(true)).then_some(id)).collect::<Vec<_>>()).unwrap_or_default()
    ).collect::<Vec<_>>()).unwrap_or_default();
    let interrupt_state = json!({"control":raw_gic["control"],"cpu_control":raw_gic["cpu_control"],
        "mask":raw_gic["mask"],"eligible":raw_gic["eligible"],"activeIds":ids("active"),
        "pendingIds":ids("pending"),"sgi_sources":raw_gic["sgi_sources"],
        "raw":{"path":gic_file,"bytes":gic_bytes.len(),"sha256":fujimod_firmware::sha256(&gic_bytes)}});
    let dmp=board_state["bus"]["devices"].as_array().ok_or("devices missing")?.iter()
        .find(|d|d["kind"]=="dmp-smaph-h").map(|d| {
            let s=&d["state"];
            json!({"status":s["status"],"deadline":s["deadline"],"generation":s["generation"],
                "command_ack":s["command_ack"],"pending":!s["pending"].is_null(),
                "path_pending":!s["path_pending"].is_null(),"regs":s["regs"]})
        });
    let mut pipeline: Vec<_> = board_state["bus"]["devices"].as_array().ok_or("devices missing")?.iter()
        .filter(|device| device["kind"] == "xpp-lens-port" || device["kind"] == "xpp-shutter" || device["kind"] == "xpp-display-output" || matches!(device["kind"].as_str(),Some("xpp-imaging-control"|"xpp-frame-sync"|"xpp-imaging-statistics"))
            || device["windows"].as_array().is_some_and(|windows| windows.iter().any(|window|window["base"].as_u64()==Some(0xfff56000))))
        .cloned().collect();
    // The lens transfer buffer can exceed 2 KiB: keep binary data out of JSON.
    for node in &mut pipeline {
        if node["kind"]!="xpp-lens-port" { continue; }
        if let Some(out)=node["state"]["link"]["out"].as_array().filter(|out|out.len()>128) {
            let bytes=out.iter().map(|v|v.as_u64().and_then(|v|u8::try_from(v).ok()).ok_or("invalid lens byte"))
                .collect::<Result<Vec<_>,_>>()?;
            let path=target.with_extension(format!("{}-{}.lens-out.bin",std::process::id(),session.core.board.now()));
            std::fs::write(&path,&bytes)?;
            node["state"]["link"]["out"]=json!({"path":path,"bytes":bytes.len(),"sha256":fujimod_firmware::sha256(&bytes)});
        }
    }
    let mut shutter_rows=Vec::new();
    for address in [0xe29b08_u64,0xe29b24] {
        let mut row=[0;28];session.core.board.read_physical(address,&mut row)?;
        shutter_rows.push(json!({"address":address,"key":row[0],"flags":u32::from_le_bytes(row[4..8].try_into().unwrap()),
            "pressed":row[21],"sample":row[22],"previous":row[23]}));
    }
    let mut irq_callbacks=Vec::new();
    for irq in [98_u64,100,101,102,103,104,106,108,112,113,114,117,118,119] {
        let mut raw=[0;16];session.core.board.read_physical(0xe8328c+irq*16,&mut raw)?;
        irq_callbacks.push(json!({"irq":irq,"words":raw.chunks_exact(4).map(|v|u32::from_le_bytes(v.try_into().unwrap())).collect::<Vec<_>>()}));
    }
    let mut ui_startup=serde_json::Map::new();
    for (name,address,bytes) in [("recInitMachineState",0x1a61b70,2),("menuPermitRaw",0x10aa094,1),("keyMachineState",0x1a61bcc,2),("phase",0x10a72ec,2),("expectedInitComplete",0x10a7378,4),
        ("modeReason",0x10a7fc1,1),("setupFlags",0x10a82bf,1),("libraryStatus",0x10aca78,4)] {
        let mut raw=[0;4];session.core.board.read_physical(address,&mut raw[..bytes])?;
        ui_startup.insert(name.into(),json!(u32::from_le_bytes(raw)));
    }
    let mut lens_data=serde_json::Map::new();
    for (name,address,size) in [("error",0x10c3a28,4),("count",0x10c3a2c,2),
        ("requestedIndex",0x10c3a2e,2),("receivedIndex",0x10c3a30,2),
        ("buffer",0x10c3a34,4),("replyCount",0x10c3a44,2),
        ("callbackFlag",0x10c3a46,2),("endMarker",0x10c3a48,2),
        ("serialMachineState",0xff395c,1),("serialCommand",0x10bd004,2)] {
        let mut raw=[0;4];session.core.board.read_physical(address,&mut raw[..size])?;
        lens_data.insert(name.into(),json!(u32::from_le_bytes(raw)));
    }
    let mut rec_states=serde_json::Map::new();
    let mut rec_events=serde_json::Map::new();
    let mut rec_active=serde_json::Map::new();
    for id in 0_u64..100 {
        let mut raw=[0;2];session.core.board.read_physical(0x1b00c64+id*2,&mut raw)?;
        let state=u16::from_le_bytes(raw);
        session.core.board.read_physical(0x1b2950c+id*2,&mut raw)?;
        let event=u16::from_le_bytes(raw);
        if [4,5,0x42,0x52,0x56].contains(&id) {
            rec_states.insert(format!("{id:x}"),json!(state));
            rec_events.insert(format!("{id:x}"),json!(event));
        }
        if state!=0 { rec_active.insert(format!("{id:x}"),json!({"state":state,"event":event})); }
    }
    let mut key_globals = serde_json::Map::new();
    for (name, address) in [("sendError",0x10bd6b4), ("sender",0xe32328), ("client",0xe87acc), ("clientState",0xe87a7c), ("queued",0xe87ad4), ("special",0xe87984)] {
        let mut word=[0;4];
        session.core.board.read_physical(address,&mut word)?;
        key_globals.insert(name.into(),json!(u32::from_le_bytes(word)));
    }
    let mut states = [0; 24];
    session.core.board.read_physical(0x1a61b40, &mut states)?;
    let exported_after=if let Ok(name)=std::env::var("XPRO2_EXPORT_AFTER") {
        let relative=std::path::Path::new(&name);
        if !relative.starts_with("analysis/snapshots") || relative.components().any(|c|matches!(c,std::path::Component::ParentDir)) {
            return Err("export must be a new file under analysis/snapshots".into());
        }
        let path=root.join(relative);
        let before=serde_json::to_value(session.fingerprint()?)?;
        let bytes=session.export_snapshot()?.snapshot;
        let restored=Session::load_snapshot(&bytes)?;
        if serde_json::to_value(restored.fingerprint()?)?!=before { return Err("after-control snapshot roundtrip differs".into()); }
        write_new_snapshot(&path,&bytes)?;
        Some(json!({"path":path,"bytes":bytes.len(),"sha256":fujimod_firmware::sha256(&bytes),"roundtripVerified":true}))
    }else{None};
    let cards: Vec<_> = session
        .core
        .contacts
        .keys()
        .map(|name| json!({"node":name,"state":session.core.card_state(name)}))
        .collect();
    let mut diagnostic_words=serde_json::Map::new();
    let mut virtual_diagnostics=serde_json::Map::new();
    if let Ok(addresses)=std::env::var("XPRO2_READ_WORDS") {
        if addresses.split(',').count()>32 { return Err("too many diagnostic words".into()); }
        for address in addresses.split(',') {
            let address=u64::from_str_radix(address.trim().trim_start_matches("0x"),16)?;
            if address>0x1fff_fffc || address&3!=0 { return Err("diagnostic word must be aligned RAM".into()); }
            let mut raw=[0;4];session.core.board.read_physical(address,&mut raw)?;
            diagnostic_words.insert(format!("{address:x}"),json!(u32::from_le_bytes(raw)));
            if std::env::var("XPRO2_COMPARE_VIRTUAL").as_deref()==Ok("1") {
                let mut virtual_raw=[0;4];
                let result=session.core.board.cpu(0)?.read_virtual(address,
                    fujimod_emulator_cpu::Access::Read,&mut virtual_raw);
                let value=match result {
                    Ok(())=>json!({"word":u32::from_le_bytes(virtual_raw),"sameAsPhysical":raw==virtual_raw}),
                    Err(error)=>json!({"error":error}),
                };
                virtual_diagnostics.insert(format!("{address:x}"),value);
                if std::env::var("XPRO2_COMPARE_ALIASES").as_deref()==Ok("1") {
                    let alias=address|0x4000_0000;let mut alias_raw=[0;4];
                    let result=session.core.board.cpu(0)?.read_virtual(alias,
                        fujimod_emulator_cpu::Access::Read,&mut alias_raw);
                    let value=match result {
                        Ok(())=>json!({"word":u32::from_le_bytes(alias_raw),"sameAsPhysical":raw==alias_raw}),
                        Err(error)=>json!({"error":error}),
                    };
                    virtual_diagnostics.insert(format!("{alias:x}"),value);
                }
            }
        }
    }
    let ram_export = if let Ok(name)=std::env::var("XPRO2_EXPORT_RAM") {
        let path=root.join(name);
        if !path.starts_with(root.join("analysis/snapshots")) { return Err("RAM export must be in analysis/snapshots".into()); }
        let base=std::env::var("XPRO2_EXPORT_RAM_BASE").unwrap_or("0".into()).parse::<u64>()?;
        let size=std::env::var("XPRO2_EXPORT_RAM_SIZE").unwrap_or("50331648".into()).parse::<usize>()?;
        if size==0 || size>64*1024*1024 || base.checked_add(size as u64).is_none_or(|end|end>0x40000000) {
            return Err("invalid bounded RAM export".into());
        }
        let mut bytes=vec![0;size];session.core.board.read_physical(base,&mut bytes)?;
        use std::io::Write;
        let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
        file.write_all(&bytes)?;
        Some(json!({"path":path,"base":base,"bytes":size,"sha256":fujimod_firmware::sha256(&bytes)}))
    } else { None };
    let lcd = if std::env::var("XPRO2_DUMP_LCD").as_deref()==Ok("1") {
        if let Some(frame)=session.frame(0)? {
            let path=target.with_extension(format!("{}-lcd.rgba",std::process::id()));
            std::fs::write(&path,&frame.rgba)?;
            Some(json!({"path":path,"width":frame.width,"height":frame.height,
                "sha256":fujimod_firmware::sha256(&frame.rgba)}))
        } else { None }
    } else { None };
    let preference_file=if std::env::var("XPRO2_READ_PREF_FILE").as_deref()==Ok("1") {
        let limit=std::env::var("XPRO2_PREF_FILE_BYTES").unwrap_or("16".into()).parse::<usize>()?;
        if !(4..=128).contains(&limit) || limit%4!=0 {return Err("preference file capture must be4..128 aligned bytes".into());}
        Some(session.core.card_read("sd","/OPACITY.BIN",None,limit).map(|bytes|json!({
            "bytes":bytes.len(),"sha256":fujimod_firmware::sha256(&bytes),
            "words":bytes.chunks_exact(4).map(|v|u32::from_le_bytes(v.try_into().unwrap())).collect::<Vec<_>>()
        })))
    }else{None};
    let mut report=json!({
            "reused":reused,"snapshot":args[3],"exportedAfter":exported_after,"lcd":lcd,"ramExport":ram_export,"gpio":gpio_export,"devices":devices_export,"armException":arm_exception,"controlRuns":control_runs,"coldCycle":cold_cycle,
            "registers":if std::env::var("XPRO2_DUMP_REGISTERS").as_deref()==Ok("1") {Some((0..2).map(|core|(0..16).map(|index|session.core.board.read_register(core,fujimod_emulator_cpu::Register::Arm32(fujimod_emulator_cpu::arm::Register::R(index)))).collect::<Vec<_>>()).collect::<Vec<_>>())}else{None},
            "timers":board_state["bus"]["devices"].as_array().and_then(|devices|devices.iter().find(|device|device["kind"]=="xpp-timers")).map(|device|device["state"].clone()),
            "snapshotSha256":fujimod_firmware::sha256(&std::fs::read(&target)?),
            "tick":session.core.board.now(),"instructions":session.core.board.instruction_counts(),
            "uiMachineStates":states.chunks_exact(2).map(|b|u16::from_le_bytes([b[0],b[1]])).collect::<Vec<_>>(),
            "uiWrites":ui_writes,"uiInstructions":ui_instructions,"osdBefore":osd_before,"osdAfter":osd_after,"pipeline":pipeline,"gic":interrupt_state,"pcs":(0..2).map(|core|session.core.board.read_register(core,fujimod_emulator_cpu::Register::Pc)).collect::<Vec<_>>(),
            "dmp":dmp,"adcSamples":session.core.adc_samples("adc")?,"lensGpio":lens_gpio,"diagnosticWords":diagnostic_words,"virtualDiagnostics":virtual_diagnostics,"recActiveMachines":rec_active,"recStates":rec_states,"recEvents":rec_events,"lensData":lens_data,"taskStates":task_states,"uiStartup":ui_startup,"shutterRows":shutter_rows,"irqCallbacks":irq_callbacks,"keyGlobals":key_globals,"rowProbe":row_probe,"cards":cards,"pcEntries":session.core.board.pc_entries(),"state":session.session_state()?
        });
    report["preferenceFile"]=serde_json::to_value(preference_file)?;
    report["guideCases"]=serde_json::to_value(guide_cases)?;
    report["armExceptionsByCore"]=serde_json::to_value((0..2).map(|core| {
        let read=|crn,opc2| session.core.board.cpu(core)?.coprocessor(
            fujimod_emulator_cpu::arm::Coprocessor{cp:15,opc1:0,crn,crm:0,opc2,secure:0,wide:false},None);
        Ok::<_,String>(json!({"core":core,"dfsr":read(5,0)?,"ifsr":read(5,1)?,"dfar":read(6,0)?,"ifar":read(6,2)?}))
    }).collect::<Result<Vec<_>,_>>()?)?;
    println!("{}",serde_json::to_string_pretty(&report)?);
    Ok(())
}
