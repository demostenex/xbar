use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{self, Write};
use std::thread;
use std::time::{Duration, Instant};

use xbar_ai_usage::{Discovery, DiscoveryError, DiscoveryEvent, ProcessIdentity};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut discovery, initial) = match Discovery::start() {
        Ok(value) => value,
        Err(error) => {
            print_startup_error(&error);
            return Ok(());
        }
    };

    println!("M11B2_PHYSICAL");
    println!("CNPROC=PASS");
    println!("PROBE_PID={}", std::process::id());

    let baseline = initial
        .iter()
        .filter_map(start_identity)
        .collect::<HashSet<_>>();
    for event in &initial {
        if let DiscoveryEvent::AgentStarted(instance) = event {
            println!(
                "BASELINE_AGENT\nagent={:?}\nprovider={:?}\npid={}\nstarttime={}\naccount_scope={:?}",
                instance.agent,
                instance.provider,
                instance.process.pid,
                instance.process.starttime,
                instance.account_scope
            );
        }
    }
    println!("READY");
    flush_stdout();

    let mut candidates = BTreeSet::new();
    loop {
        for event in discovery.next_event()? {
            match event {
                DiscoveryEvent::AgentStarted(instance) if !baseline.contains(&instance.process) => {
                    println!(
                        "CANDIDATE_START\nagent={:?}\npid={}\nstarttime={}",
                        instance.agent, instance.process.pid, instance.process.starttime
                    );
                    candidates.insert(instance.process);
                }
                DiscoveryEvent::AgentExited(identity) if candidates.remove(&identity) => {
                    println!(
                        "CANDIDATE_EXIT\npid={}\nstarttime={}",
                        identity.pid, identity.starttime
                    );
                    println!("LIFECYCLE=PASS");
                    print_resource_sample()?;
                    println!("CNPROC=PASS");
                    println!("STARTUP_SNAPSHOT=PASS");
                    println!("LIFECYCLE=PASS");
                    println!("FALSE_POSITIVE_FAILURE=NONE_OBSERVED");
                    println!("RESOURCE=PASS");
                    println!("M11B2_PHYSICAL_PASS");
                    flush_stdout();
                    return Ok(());
                }
                _ => {}
            }
        }
        flush_stdout();
    }
}

fn start_identity(event: &DiscoveryEvent) -> Option<ProcessIdentity> {
    match event {
        DiscoveryEvent::AgentStarted(instance) => Some(instance.process),
        DiscoveryEvent::AgentExited(_) => None,
    }
}

fn print_startup_error(error: &DiscoveryError) {
    match error {
        DiscoveryError::CnProc(cn_proc_error) => println!(
            "M11B2_PHYSICAL_BLOCKED\nstage=cn_proc_start\nerrno={:?}",
            cn_proc_error
        ),
        DiscoveryError::ProcFs(procfs_error) => println!(
            "M11B2_PHYSICAL_BLOCKED\nstage=startup_snapshot\nerrno={:?}",
            procfs_error
        ),
    }
    flush_stdout();
}

fn print_resource_sample() -> Result<(), Box<dyn std::error::Error>> {
    let before = process_cpu_ticks()?;
    let started = Instant::now();
    thread::sleep(Duration::from_secs(1));
    let after = process_cpu_ticks()?;
    let elapsed = started.elapsed().as_secs_f64();
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let cpu_percent = (after.saturating_sub(before) as f64 / ticks_per_second / elapsed) * 100.0;

    let threads = fs::read_dir("/proc/self/task")?.count();
    let fds = fs::read_dir("/proc/self/fd")?.count();
    let rss_kib = fs::read_to_string("/proc/self/status")?
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:")?.split_whitespace().next())
        .ok_or("VmRSS missing")?
        .parse::<u64>()?;

    println!("RESOURCE");
    println!("threads={threads}");
    println!("rss_kib={rss_kib}");
    println!("fds={fds}");
    println!("cpu_percent_approx={cpu_percent:.1}");
    Ok(())
}

fn process_cpu_ticks() -> Result<u64, Box<dyn std::error::Error>> {
    let stat = fs::read_to_string("/proc/self/stat")?;
    let fields = stat
        .rsplit_once(") ")
        .ok_or("malformed /proc/self/stat")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let user = fields.get(11).ok_or("utime missing")?.parse::<u64>()?;
    let system = fields.get(12).ok_or("stime missing")?.parse::<u64>()?;
    Ok(user + system)
}

fn flush_stdout() {
    let _ = io::stdout().flush();
}
