//! Experimental X-Pro2 loader with the existing XPP device graph.
use fujimod_catalog_resolution::ResolvedConfiguration;
use fujimod_emulator::board::{bus::Mode, factory};
use fujimod_emulator_boot::{graph::ProcessorStart, xpp};
use fujimod_emulator_cpu::Register;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::args().nth(1).ok_or("workspace root required")?);
    let limit: u64 = std::env::args()
        .nth(2)
        .unwrap_or("1000000".into())
        .parse()?;
    let ram_bytes: usize = std::env::args().nth(3).unwrap_or("1073741824".into()).parse()?;
    let tick_limit: u64 = std::env::args().nth(4).unwrap_or("100000000".into()).parse()?;
    if tick_limit == 0 { return Err("zero simulated time limit".into()); }
    if ram_bytes == 0 || ram_bytes > 0x40000000 || ram_bytes % 4096 != 0 { return Err("invalid RAM size".into()); }
    let source = std::fs::read(root.join("FWUP0008_511.DAT"))?;
    if fujimod_firmware::sha256(&source)
        != "6f9792e53a776ee116732a6df4425b0f5393ad2f53655f573924afe9c61dc54d"
    {
        return Err("DAT identity mismatch".into());
    }
    let repo = root.join("references/fujimod-current");
    let kernel_scenario = std::env::args().nth(5).as_deref() == Some("kernel");
    let rom_park = std::env::args().nth(5).as_deref() == Some("rom-park");
    let sequence_end = match std::env::var("XPRO2_SEQUENCE_END_IRQ").as_deref() {
        Ok("1") => true, Err(std::env::VarError::NotPresent) | Ok("0") => false,
        _ => return Err("XPRO2_SEQUENCE_END_IRQ requires 0 or 1".into()),
    };
    if sequence_end && !rom_park { return Err("sequence-end hypothesis requires rom-park profile".into()); }
    let quantum: usize = std::env::var("XPRO2_QUANTUM").unwrap_or("4096".into()).parse()?;
    if quantum == 0 { return Err("zero quantum".into()); }
    let key_probe = std::env::var("XPRO2_KEY_PROBE").as_deref() == Ok("1");
    if key_probe && !sequence_end { return Err("key probe requires sequence profile".into()); }
    let artifact_prefix = if key_probe { "xpro2-keys-runtime" } else if sequence_end { "xpro2-sequence-runtime" } else if kernel_scenario { "xpro2-kernel-runtime" } else if rom_park { "xpro2-rompark-runtime" } else { "xpro2-runtime" };
    let mut handoff_identity = None;
    let mut assets = BTreeMap::from([(
        "xt20-211-statistics-window".into(),
        std::fs::read(repo.join("configurations/emulator/assets/xt20-211-statistics-window.bin"))?,
    )]);
    let starts: Vec<_> = (0..2)
        .map(|core| ProcessorStart {
            processor_id: format!("core{core}"),
            released: true,
            registers: vec![
                (Register::Pc, if kernel_scenario { 0x57978 } else if rom_park && core == 1 { 0x348 } else { 0 }),
                (Register::Pstate, 0xd3),
                (Register::Sp, 0x7fff000 - core * 0x1000),
                (Register::Arm32(fujimod_emulator_cpu::arm::Register::R(0)), if kernel_scenario { 0xba90a8 } else { 0 }),
                (Register::Arm32(fujimod_emulator_cpu::arm::Register::R(1)), if kernel_scenario { 0x900000 } else { 0 }),
                (Register::Arm32(fujimod_emulator_cpu::arm::Register::R(14)), if rom_park && core == 1 { 0x120 } else { 0 }),
            ],
        })
        .collect();
    let resolved = ResolvedConfiguration::bundled("xt20-211")?;
    let assembly = xpp::assemble(
        &resolved,
        &xpp::Inputs {
            starts: &starts,
            sub_cpu_registers: &[],
            assets: &assets,
            sensor_image: None,
        },
    )?;
    let mut devices = assembly.devices;
    let reject_c0 = std::env::var("XPRO2_LENS_REJECT_C0").as_deref() == Ok("1");
    for node in devices["nodes"].as_array_mut().ok_or("nodes absent")? {
        if node["model"]["kind"] == "clock-control" {
            node["model"]["options"]["running"] = json!([0, 1, 2, 3, 4]);
        }
        if reject_c0 && node["model"]["kind"] == "lens" {
            node["model"]["rejected_commands"] = json!([0xc0]);
        }
        if node["model"]["kind"] == "flash" {
            node["model"]["transfer_ticks"] = json!(1);
        }
    }
    // Explicit second-card scenario. XPP's second SDHCI bank is at +0x100.
    let mut second_sd = devices["nodes"].as_array().ok_or("nodes")?.iter()
        .find(|n| n["model"]["kind"] == "sd").ok_or("primary SD absent")?.clone();
    second_sd["name"] = json!("sd1");
    second_sd["model"]["options"]["base"] = json!(0xfff93000_u64);
    second_sd["model"]["options"]["register_offset"] = json!(0x100);
    second_sd["model"]["irq"] = json!(84);
    second_sd["model"]["reset_channel"] = json!(1);
    devices["nodes"].as_array_mut().ok_or("nodes")?.push(second_sd);
    for (name, base, channels) in [("capture0", 0xfff2f000_u64, 5), ("capture1", 0xfff30000, 3)] {
        let model = if sequence_end {
            json!({"kind":"input-capture-sequence", "base":base, "channels":channels,"irq":if base == 0xfff2f000 {90} else {91}})
        } else { json!({"kind":"input-capture-idle", "base":base, "channels":channels}) };
        devices["nodes"].as_array_mut().ok_or("nodes")?.push(json!({"name":name,"model":model}));
    }
    let mut graph = assembly.execution;
    // Bounded explicit test RAM; this is not a claim about physical capacity.
    graph.ram = vec![
        factory::Ram {
            base: 0,
            size: ram_bytes,
        },
        factory::Ram {
            base: 0x80000000,
            size: 0x10000000,
        },
    ];
    graph.dma_mappings = vec![
        fujimod_emulator::board::memory::BusMapping::ram(0, ram_bytes),
        fujimod_emulator::board::memory::BusMapping {
            address: 0x40000000,
            physical: 0,
            size: ram_bytes,
            permissions: 1,
        },
        fujimod_emulator::board::memory::BusMapping::ram(0x80000000, 0x10000000),
    ];
    graph.mappings = (0..2)
        .flat_map(|core| {
            [
                factory::MemoryMap {
                    core,
                    address: 0,
                    physical: 0,
                    size: ram_bytes,
                    permissions: 7,
                },
                factory::MemoryMap {
                    core,
                    address: 0x40000000,
                    physical: 0,
                    size: ram_bytes,
                    permissions: 1,
                },
                factory::MemoryMap {
                    core,
                    address: 0x80000000,
                    physical: 0x80000000,
                    size: 0x10000000,
                    permissions: 7,
                },
            ]
        })
        .collect();
    assets.insert("flash".into(), fujimod_firmware::invert(&source[0x25c..]));
    let recipes: Vec<Value> = serde_json::from_slice(&std::fs::read(
        repo.join("catalog/recovery/recipes.generated.json"),
    )?)?;
    let recipe = recipes
        .iter()
        .find(|r| r["firmware_id"] == "xpro2-511")
        .ok_or("recipe absent")?;
    let mappings: Vec<_> = recipe["plan"]["regions"]
        .as_array()
        .ok_or("regions absent")?
        .iter()
        .map(|r| {
            let s = &r["source"];
            Ok(fujimod_firmware::Mapping {
                name: r["name"].as_str().ok_or("name")?.into(),
                offset: s["dat_offset"].as_u64().ok_or("offset")?,
                size: s["decoded_size"].as_u64().ok_or("size")?,
                base: r["address"].as_u64().ok_or("address")?,
                executable: true,
                encoding: if s["compression"] == "raw" {
                    fujimod_firmware::Encoding::Raw
                } else {
                    fujimod_firmware::Encoding::Method4
                },
            })
        })
        .collect::<Result<_, &str>>()?;
    let image = fujimod_firmware::decode(&source, &mappings)?;
    let firmware_mappings:Vec<_>=recipe["plan"]["regions"].as_array().ok_or("regions")?.iter().map(|r|json!({
        "name":r["name"],"offset":r["source"]["dat_offset"],"size":r["source"]["decoded_size"],"base":r["address"],
        "encoding":if r["source"]["compression"]=="raw" {"raw"} else {"method4"},"executable":true
    })).collect();
    let execution = fujimod_emulator_boot::graph::value(
        &resolved.reference,
        &graph,
        fujimod_catalog_configuration::RunInstructionRate {
            numerator: assembly.rate.0,
            denominator: assembly.rate.1,
        },
    );
    let document = json!({"schema":1,"firmwareId":"xpro2-511","graph":{
        "schema":1,"firmwareId":"xpro2-511","execution":execution,"devices":devices,
        "assets":{"flash":{"kind":"dat","offset":0x25c,"size":source.len()-0x25c,"inverted":true}},
        "firmware":{"size":source.len(),"sha256":fujimod_firmware::sha256(&source),"format":"dat","mappings":firmware_mappings},
        "quantum":quantum,"scanout":null
    }});
    std::fs::write(
        root.join(if reject_c0 { "analysis/xpro2-lens-reject-browser-graph.json" } else if sequence_end { "analysis/xpro2-sequence-browser-graph.json" } else if kernel_scenario { "analysis/xpro2-kernel-browser-graph.json" } else if rom_park { "analysis/xpro2-rompark-browser-graph.json" } else { "analysis/xpro2-browser-graph.json" }),
        serde_json::to_vec_pretty(&document)?,
    )?;
    let spec = serde_json::from_value(devices.clone())?;
    let built = fujimod_emulator_xpp::board::build(graph, spec, &assets, Mode::Strict)?;
    for unit in &image.units {
        built.board.write_physical(unit.base, &unit.bytes)?;
    }
    if kernel_scenario {
        let handoff_path = std::env::args().nth(6).unwrap_or("analysis/xpro2-kernel-handoff-scenario.bin".into());
        let handoff = std::fs::read(root.join(&handoff_path))?;
        if handoff.len() != 0x5c0 { return Err("invalid handoff size".into()); }
        handoff_identity = Some(json!({"path":handoff_path,"sha256":fujimod_firmware::sha256(&handoff)}));
        built.board.write_physical(0x900000, &handoff)?;
    }
    let mut core = fujimod_emulator_session::Session {
        definition: document.clone(),
        controls: None,
        contact_driver: None,
        board: built.board,
        resources: Default::default(),
        nodes: built.nodes,
        contacts: built.contacts,
        recording_frames: Default::default(),
    };
    if rom_park && matches!(std::env::args().nth(6).as_deref(), Some("ui-messages" | "sys-messages" | "boot-modes" | "rec-messages")) {
        let rec_messages = std::env::args().nth(6).as_deref() == Some("rec-messages");
        let system_messages = rec_messages || std::env::args().nth(6).as_deref() == Some("sys-messages");
        let boot_modes = std::env::args().nth(6).as_deref() == Some("boot-modes");
        let message_address = if rec_messages { 0x1a56428 } else if boot_modes { 0x10a7fbc } else if system_messages { 0x1a61ca8 } else { 0x109ce20 };
        let mut observations = Vec::new();
        for _ in 0..48 {
            let maps = core.board.mappings().to_vec();
            core.board.continuation_begin(vec![(message_address,message_address+if boot_modes { 1 } else { 2 }),(0x1026a48,0x1026a49)], Default::default(), maps)?;
            core.board.run_with_limits(20_000_000, quantum, None, Some(tick_limit))?;
            let stop = core.board.continuation_stop();
            if stop.is_some() {
                let mut header=[0;2]; let mut mode=[0;1];
                core.board.read_physical(message_address,&mut header)?;core.board.read_physical(0x1026a48,&mut mode)?;
                let mut observation=json!({"stop":format!("{stop:?}"),"tick":core.board.now(),"message":u16::from_le_bytes(header),"uiMode":mode[0],
                    "pc0":core.board.read_register(0,Register::Pc)?,"pc1":core.board.read_register(1,Register::Pc)?,"instructions":core.board.instruction_counts()});
                // Stock receive wrapper ba103c..ba1074 commits the 16-bit
                // message tag first. Read its complete queued source packet,
                // rather than source/length bytes not yet copied to the target.
                for cpu in 0..2 {
                    let pc = core.board.read_register(cpu, Register::Pc)?;
                    let packet = if pc == 0xba1048 {
                        Some(core.board.read_register(cpu, Register::Arm32(fujimod_emulator_cpu::arm::Register::R(1)))?)
                    } else if !system_messages && pc == 0x1448158 {
                        // The UI library receives into a local queue slot, then
                        // copies the header to its separate public message buffer.
                        let fp = core.board.read_register(cpu, Register::Arm32(fujimod_emulator_cpu::arm::Register::R(11)))?;
                        let mut slot = [0; 4];
                        core.board.read_physical(fp - 0x10, &mut slot)?;
                        Some(u64::from(u32::from_le_bytes(slot)))
                    } else { None };
                    if let Some(packet) = packet {
                        let mut data=[0;8];core.board.read_physical(packet,&mut data)?;
                        let second_word = u32::from_le_bytes(data[4..8].try_into()?);
                        // System packets carry a length; UI-library packets have
                        // a second header word followed by a fixed 0x44-byte body.
                        let prefix_len = if system_messages { second_word.min(16) as usize } else { 16 };
                        let mut payload = vec![0; prefix_len];
                        core.board.read_physical(packet + 8, &mut payload)?;
                        observation["packet"] = json!({"address":packet,"code":u16::from_le_bytes(data[..2].try_into()?),"byte2":data[2],"byte3":data[3],"secondWord":second_word,"format":if system_messages { "system" } else { "ui-library" },"payloadPrefix":payload});
                    }
                }
                if boot_modes {
                    let mut registers = Vec::new();
                    for cpu in 0..2 {
                        let values = (0..4).chain([11, 14]).map(|r| core.board.read_register(cpu, Register::Arm32(fujimod_emulator_cpu::arm::Register::R(r)))).collect::<Result<Vec<_>, _>>()?;
                        registers.push(values);
                    }
                    observation["registersR0R1R2R3FpLr"] = json!(registers);
                }
                let mut machine_states = [0; 24];
                core.board.read_physical(0x1a61b40, &mut machine_states)?;
                observation["uiMachineStates"] = json!(machine_states.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect::<Vec<_>>());
                for (name, address, length) in [
                    ("uiPhase", 0x10a72ec, 2), ("bootMode", 0x10a7fbc, 1),
                    ("modeReason", 0x10a7fc1, 1), ("setupFlags", 0x10a82bf, 1),
                    ("expectedInitComplete", 0x10a7378, 4), ("libraryStatus", 0x10aca78, 4),
                    ("uiSetupState", 0x1a61bfe, 2), ("recDisplayState", 0x1b00c6c, 2),
                    ("pendingLibraryToken", 0x1900bec, 4),
                ] {
                    let mut bytes = [0; 4];
                    core.board.read_physical(address, &mut bytes[..length])?;
                    observation[name] = json!(u32::from_le_bytes(bytes));
                }
                observations.push(observation);
            }
            core.board.continuation_end()?;
            if core.board.now()>=tick_limit {break;}
        }
        println!("{}",serde_json::to_string_pretty(&json!({"messageAddress":message_address,"observations":observations,"tick":core.board.now(),"completeCameraBoot":false,"sequenceEndProfile":sequence_end}))?);
        return Ok(());
    }
    if std::env::args().nth(5).as_deref() == Some("causewrites") {
        let mut observations = Vec::new();
        for _ in 0..8 {
            let maps = core.board.mappings().to_vec();
            core.board.continuation_begin(vec![(0x1ad68,0x1ad6c),(0x7f51c,0x7f520)], Default::default(), maps)?;
            core.board.run_with_limits(20_000_000, quantum, None, Some(10_000_000))?;
            let stop = core.board.continuation_stop();
            let mut source = [0;4]; let mut destination = [0;4];
            core.board.read_physical(0x1ad68, &mut source)?;
            core.board.read_physical(0x7f51c, &mut destination)?;
            observations.push(json!({"stop":format!("{stop:?}"),"source":u32::from_le_bytes(source),"destination":u32::from_le_bytes(destination),
                "pc0":core.board.read_register(0,Register::Pc)?,"pc1":core.board.read_register(1,Register::Pc)?,"instructions":core.board.instruction_counts()}));
            core.board.continuation_end()?;
            if stop.is_none() { break; }
        }
        println!("{}",serde_json::to_string_pretty(&json!({"observations":observations,"completeCameraBoot":false,"sequenceEndProfile":sequence_end}))?);
        return Ok(());
    }
    if std::env::args().nth(5).as_deref() == Some("powercause") {
        let mut hit_core = None;
        let mut path = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut previous = [u64::MAX; 2];
        for _ in 0..9_000_000 {
            for cpu in 0..2 {
                let pc = core.board.read_register(cpu, Register::Pc)?;
                if ((0x100..0x180).contains(&pc) || (0x348..0x374).contains(&pc) || (0x6e0..0x7a0).contains(&pc)) && pc != previous[cpu] && seen.insert((cpu,pc)) {
                    path.push(json!({"core":cpu,"pc":pc,"r0":core.board.read_register(cpu,Register::Arm32(fujimod_emulator_cpu::arm::Register::R(0)))?,"r1":core.board.read_register(cpu,Register::Arm32(fujimod_emulator_cpu::arm::Register::R(1)))?}));
                }
                previous[cpu] = pc;
                if pc == 0x774 { hit_core = Some(cpu); }
            }
            if hit_core.is_some() { break; }
            core.board.run_for_instructions(1, quantum)?;
        }
        let state = core.board.state();
        let ports: Vec<_> = state["bus"]["devices"].as_array().ok_or("devices")?.iter()
            .filter(|n| n["kind"] == "xpp-serial").map(|n| json!({
                "windows":n["windows"], "powerCause":n["state"]["peer"][96],
                "send":n["state"]["registers"][24], "status":n["state"]["registers"][10]
            })).collect();
        println!("{}", serde_json::to_string_pretty(&json!({
            "hitPowerCauseStore":hit_core.is_some(),"hitCore":hit_core,"path":path,"pc":core.board.read_register(hit_core.unwrap_or(0),Register::Pc)?,
            "returnValue":core.board.read_register(hit_core.unwrap_or(0),Register::Arm32(fujimod_emulator_cpu::arm::Register::R(0)))?,
            "ports":ports,"instructions":core.board.instruction_counts(),"completeCameraBoot":false
        }))?);
        return Ok(());
    }
    let mut result = Ok(());
    let mut remaining = limit;
    let mut frames = Vec::new();
    while remaining > 0 {
        let step = remaining.min(10_000_000);
        result = core.board.run_with_limits(step, quantum, None, Some(tick_limit));
        remaining -= step;
        frames.extend(core.board.take_frames());
        eprintln!("X-Pro2 tick={} instructions={:?} frames={}", core.board.now(), core.board.instruction_counts(), frames.len());
        if result.is_err() || core.board.now() >= tick_limit { break; }
    }
    let mut key_observations = Vec::new();
    if key_probe && result.is_ok() {
        let idle = core.adc_samples("adc")?;
        let navigation_probe = std::env::var("XPRO2_NAVIGATION_PROBE").as_deref() == Ok("1");
        let mut keys = if navigation_probe {
            vec![("ok",328,0xe29dfc_u64),("back",706,0xe29da8)]
        } else { vec![("down",62,0xe29d1c_u64),("up",15,0xe29d00)] };
        if std::env::var("XPRO2_POST_SETUP_MENU").as_deref() == Ok("1") {
            keys.push(("shooting-menu",328,0xe29dfc));
        }
        'keys: for (name, sample, row) in keys {
            let mut pressed = idle.clone();
            pressed[3] = sample;
            for (phase, values) in [("press", &pressed), ("release", &idle)] {
                core.set_adc_samples("adc", values)?;
                let until = core.board.now() + 4_800_000;
                let mut messages = Vec::new();
                while core.board.now() < until {
                    let maps = core.board.mappings().to_vec();
                    core.board.continuation_begin(vec![(0x109ce20,0x109ce22)], Default::default(), maps)?;
                    result = core.board.run_with_limits(20_000_000, quantum, None, Some(until));
                    if result.is_err() {
                        core.board.continuation_end()?;
                        break 'keys;
                    }
                    if core.board.continuation_stop().is_some() {
                        let mut tag = [0;2]; core.board.read_physical(0x109ce20, &mut tag)?;
                        messages.push(u16::from_le_bytes(tag));
                    }
                    core.board.continuation_end()?;
                }
                let mut stable = [0]; core.board.read_physical(row + 21, &mut stable)?;
                key_observations.push(json!({"key":name,"phase":phase,"adc3":values[3],"stable":stable[0],"messages":messages,"tick":core.board.now()}));
            }
            let settle = if navigation_probe { 96_000_000 } else { 24_000_000 };
            result = core.board.run_with_limits(200_000_000, quantum, None, Some(core.board.now()+settle));
            if result.is_err() { break; }
            let produced = core.board.take_frames();
            if let Some(frame) = produced.last() {
                let path = format!("analysis/{artifact_prefix}-{name}.rgba");
                std::fs::write(root.join(&path), &frame.rgba)?;
                key_observations.push(json!({"key":name,"frame":{"path":path,"width":frame.width,"height":frame.height,"sha256":fujimod_firmware::sha256(&frame.rgba)}}));
            }
            frames.extend(produced);
        }
    }
    let mut dma_programs = vec![0; 0x8000];
    core.board.read_physical(0xe74140, &mut dma_programs)?;
    std::fs::write(root.join(format!("analysis/{artifact_prefix}-pl330-programs.bin")), &dma_programs)?;
    let mut kernel_ram = vec![0; 0x100000];
    core.board.read_physical(0x40000, &mut kernel_ram)?;
    std::fs::write(root.join(format!("analysis/{artifact_prefix}-kernel-ram.bin")), &kernel_ram)?;
    let mut task_stacks = vec![0; 0xc0000];
    core.board.read_physical(0xf40000, &mut task_stacks)?;
    std::fs::write(root.join(format!("analysis/{artifact_prefix}-task-stacks.bin")), &task_stacks)?;
    let pcs: Vec<_> = (0..2)
        .map(|index| {
            core.board
                .read_register(index, Register::Pc)
                .map(|v| format!("{v:#x}"))
        })
        .collect();
    frames.extend(core.board.take_frames());
    if let Some(f) = frames.last() {
        std::fs::write(root.join(format!("analysis/{artifact_prefix}-frame.rgba")), &f.rgba)?;
    } else {
        match std::fs::remove_file(root.join(format!("analysis/{artifact_prefix}-frame.rgba"))) {
            Ok(()) => {},
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e.into()),
        }
    }
    let frame = frames.last().map(|f| json!({"width":f.width,"height":f.height,"rgbaSha256":fujimod_firmware::sha256(&f.rgba)}));
    let registers:Vec<_>=(0..2).map(|core_id| (0..15).map(|r|core.board.read_register(core_id,Register::Arm32(fujimod_emulator_cpu::arm::Register::R(r))).map(|v|format!("{v:#x}"))).collect::<Vec<_>>()).collect();
    let console = core.board.take_console();
    let board_state = core.board.state();
    let mut diagnostics: Vec<_> = board_state["bus"]["devices"].as_array().ok_or("device states absent")?.iter()
        .filter(|node| matches!(node["kind"].as_str(), Some("xpp-lens-port" | "pl330" | "xpp-timers" | "xpp-input-capture-idle" | "xpp-input-capture-sequence" | "dmp-smaph-h" | "xpp-display-output"))).cloned().collect();
    for node in &mut diagnostics {
        if node["kind"] != "xpp-lens-port" { continue; }
        for field in ["out", "transmitted"] {
            if node["state"]["link"][field].as_array().is_some_and(|v| v.len() > 128) {
                let bytes: Vec<u8> = serde_json::from_value(node["state"]["link"][field].take())?;
                let digest = fujimod_firmware::sha256(&bytes);
                let path = format!("analysis/blobs/{digest}.bin");
                std::fs::create_dir_all(root.join("analysis/blobs"))?;
                std::fs::write(root.join(&path), &bytes)?;
                node["state"]["link"][field] = json!({"path":path,"bytes":bytes.len(),"sha256":digest});
            }
        }
    }
    // Pending renderer writes are opaque bytes, not JSON diagnostics. Store
    // content-addressed sidecars so reports stay compact and preserve identity.
    for node in &mut diagnostics {
        if node["kind"] != "dmp-smaph-h" { continue; }
        if let Some(writes) = node["state"]["pending"]["writes"].as_array_mut() {
            for write in writes {
                let bytes: Vec<u8> = serde_json::from_value(write[1].take())?;
                let digest = fujimod_firmware::sha256(&bytes);
                let path = format!("analysis/blobs/{digest}.bin");
                std::fs::create_dir_all(root.join("analysis/blobs"))?;
                let target = root.join(&path);
                if target.exists() {
                    if fujimod_firmware::sha256(&std::fs::read(&target)?) != digest { return Err("renderer sidecar identity mismatch".into()); }
                } else { std::fs::write(target, &bytes)?; }
                write[1] = json!({"path":path,"bytes":bytes.len(),"sha256":digest});
            }
        }
    }
    let report = json!({"registers":registers,"console":console,"sourceSha256":fujimod_firmware::sha256(&source),"frame":frame,"scope":"X-Pro2 DAT; X-T20 device inputs and synthetic startup, unverified physical correspondence","instructions":core.board.instruction_counts(),"tick":core.board.now(),"pc":pcs,"result":result,"completeCameraBoot":false});
    let mut report = report;
    report["bootScenario"] = json!(if kernel_scenario { "explicit-kernel-handoff" } else if rom_park { "secondary-parked-in-stock-wfi" } else { "original-loader" });
    report["keyProbe"] = json!(key_observations);
    let mut ui_states = [0; 24];
    core.board.read_physical(0x1a61b40, &mut ui_states)?;
    report["uiMachineStates"] = json!(ui_states.chunks_exact(2).map(|b| u16::from_le_bytes([b[0],b[1]])).collect::<Vec<_>>());
    report["sdCards"] = json!(core.contacts.keys().map(|name| {
        json!({"node":name,"state":core.card_state(name)})
    }).collect::<Vec<_>>());
    report["sequenceEndProfile"] = json!(sequence_end);
    if sequence_end {
        report["sequenceEndAssumption"] = json!({"terminalWord":0x00070000,"status":1,"irqs":[90,91],"timing":"next device boundary; unmeasured","hardwareValidated":false});
    }
    report["quantum"] = json!(quantum);
    report["kernelBootCause"] = json!(u32::from_le_bytes(kernel_ram[0x3f51c..0x3f520].try_into()?));
    let mut startup = serde_json::Map::new();
    for (name, address, size) in [
        ("cachedPowerCause", 0xe86f28, 4), ("powerMode", 0xe86f2d, 1),
        ("powerSourceLow", 0xe86f30, 4), ("powerClassMask", 0xe86f34, 4),
        ("uiModeRaw", 0x1026a48, 1), ("uiLastMessage", 0x109ce20, 4),
        ("bootMode", 0x10a7fbc, 1), ("uiSetupState", 0x1a61bfe, 2),
        ("recDisplayState", 0x1b00c6c, 2), ("recStage66State", 0x1b00ce8, 2),
        ("recReadyFlag", 0x1a8f78d, 1), ("recJobMode", 0x1b20f78, 1),
        ("pendingLibraryToken", 0x1900bec, 4),
        ("kernelBootKind", 0x7f998, 4),
    ] {
        let mut bytes = [0;4]; core.board.read_physical(address, &mut bytes[..size])?;
        startup.insert(name.into(), json!(u32::from_le_bytes(bytes)));
    }
    report["startup"] = json!(startup);
    let mut calibration_pointer = [0; 4];
    core.board.read_physical(0xea7eb8, &mut calibration_pointer)?;
    let calibration_base = u32::from_le_bytes(calibration_pointer) as u64;
    if calibration_base != 0 {
        let mut calibration = vec![0; 0x4000];
        core.board.read_physical(calibration_base, &mut calibration)?;
        let path = format!("analysis/{artifact_prefix}-key-calibration.bin");
        std::fs::write(root.join(&path), &calibration)?;
        report["keyCalibration"] = json!({"base":calibration_base,"bytes":calibration.len(),"path":path,"sha256":fujimod_firmware::sha256(&calibration)});
    }
    let mut key_rows = vec![0; 0x1000];
    core.board.read_physical(0xe29b00, &mut key_rows)?;
    std::fs::write(root.join(format!("analysis/{artifact_prefix}-key-rows.bin")), &key_rows)?;

    if let Some(display) = diagnostics.iter().find(|d| d["kind"] == "xpp-display-output") {
        if let Some(words) = display["state"]["words"].as_array() {
            let word = |offset:u64| words.iter().find(|w|w[0].as_u64()==Some(offset)).and_then(|w|w[1].as_u64()).unwrap_or(0);
            let address = word(0x230); let size = word(0x220);
            let width = ((size>>16)&0xfff)+1; let height=(size&0xfff)+1;
            if address != 0 && width*height<=4_194_304 {
                let mut pixels=vec![0; (width*height*4) as usize];
                match core.board.read_physical(address, &mut pixels) {
                    Ok(()) => {
                        let path=format!("analysis/{artifact_prefix}-offscreen.rgba");
                        std::fs::write(root.join(&path), &pixels)?;
                        report["offscreen"] = json!({"path":path,"address":address,"width":width,"height":height,
                            "nonzeroBytes":pixels.iter().filter(|&&v|v!=0).count(),"sha256":fujimod_firmware::sha256(&pixels),"osdGroupEnabled":word(0)&4!=0,"scanoutEvidence":false});
                    },
                    Err(error) => report["offscreen"] = json!({"error":error}),
                }
            }
        }
    }
    report["handoffInput"] = json!(handoff_identity);
    report["waiting"] = board_state["waiting"].clone();
    report["diagnostics"] = json!(diagnostics);
    report["tickLimit"] = json!(tick_limit);
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
