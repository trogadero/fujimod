//! Measure a declared scheduler batch from its own retained camera state.
use fujimod_emulator_javascript::Session;
use serde_json::{Value,json};
use std::{path::PathBuf,time::Instant};

fn advance(s:&mut Session, quantum:usize, ticks:u64)->Result<(),String>{
    let start:u64=s.core.board.instruction_counts().iter().sum();
    s.core.run_with_limits(u64::MAX-start,quantum,None,Some(s.core.board.now()+ticks))
}
fn words(s:&Session,address:u64,count:usize)->Result<Vec<u32>,String>{
    let mut raw=vec![0;count*4];s.core.board.read_physical(address,&mut raw)?;
    Ok(raw.chunks_exact(4).map(|v|u32::from_le_bytes(v.try_into().unwrap())).collect())
}
fn osd(s:&Session)->Result<String,String>{
    let state=s.core.board.state();
    let node=state["bus"]["devices"].as_array().ok_or("devices")?.iter()
        .find(|v|v["kind"]=="xpp-display-output").ok_or("display")?;
    let w=node["state"]["words"].as_array().ok_or("words")?;
    let word=|at:u64|w.iter().find(|v|v[0].as_u64()==Some(at)).and_then(|v|v[1].as_u64()).unwrap_or(0);
    let size=word(0x220);let n=(((size>>16)&0xfff)+1)*((size&0xfff)+1)*4;
    if n>16_000_000 || word(0x230)==0{return Err("invalid output".into());}
    let mut raw=vec![0;n as usize];s.core.board.read_physical(word(0x230),&mut raw)?;
    Ok(fujimod_firmware::sha256(&raw))
}
fn main()->Result<(),Box<dyn std::error::Error>>{
    let args:Vec<_>=std::env::args().collect();
    if args.len()!=3{return Err("SNAPSHOT OUTPUT.json".into());}
    let bytes=std::fs::read(&args[1])?;let mut results:Vec<Value>=vec![];
    // A running board fixes its quantum. Never change it underneath a loaded
    // snapshot; another candidate needs a fresh graph/boot of its own.
    let declared=Session::load_snapshot(&bytes)?.core.definition["graph"]["quantum"]
        .as_u64().ok_or("snapshot has no declared scheduler quantum")? as usize;
    for quantum in [declared]{
        let mut s=Session::load_snapshot(&bytes)?;
        let before=words(&s,0x1003980,8)?;let tick=s.core.board.now();
        let counts=s.core.board.instruction_counts().to_vec();let clock=Instant::now();
        for control in ["selector.down","selector.up"]{
            s.press(control.into(),None)?;advance(&mut s,quantum,4_800_000)?;
            s.release(control.into())?;advance(&mut s,quantum,4_800_000)?;
            advance(&mut s,quantum,12_000_000)?;
        }
        let elapsed=clock.elapsed().as_secs_f64();
        let row=json!({"quantum":quantum,"seconds":elapsed,"guest_ticks":s.core.board.now()-tick,
            "instructions_before":counts,"instructions_after":s.core.board.instruction_counts(),
            "menu_before":before,"menu_after":words(&s,0x1003980,8)?,
            "opacity":words(&s,0x103b00,4)?,"theme":words(&s,0x90700,1)?,
            "preferences":words(&s,0x102400,18)?,"osd_sha256":osd(&s)?});
        eprintln!("quantum {quantum}: {elapsed:.3}s");results.push(row);
    }
    let baseline=results[0]["seconds"].as_f64().ok_or("baseline")?;
    for row in &mut results{
        row["speedup"]=json!(baseline/row["seconds"].as_f64().ok_or("timing")?);
    }
    std::fs::write(PathBuf::from(&args[2]),serde_json::to_vec_pretty(&json!({
        "schema":1,"snapshot_sha256":fujimod_firmware::sha256(&bytes),"results":results,
        "scope":"Native scheduler benchmark; browser speed requires separate verification",
        "firmware_and_timer_rate_changed":false
    }))?)?;
    Ok(())
}
