use {
    anyhow::anyhow,
    rust_memtester::{MemtestReportList, Memtester, MemtesterArgs},
    std::{
        mem::size_of,
        time::{Duration, Instant},
    },
    tracing::{error, info},
};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::stderr)
        .init();
    let start_time = Instant::now();
    let (mem_usize_count, memtester_args) = match parse_args(std::env::args().skip(1)) {
        Ok((count, args)) => (count, args),
        Err(s) => {
            error!(concat!(
                "Usage: rust-memtester ",
                "<memsize in MB> ",
                "<timeout in ms> ",
                "<require_memlock as bool> ",
                "<allow_working_set_resize as bool> ",
                "<allow_mem_resize as bool> ",
                "<allow_multithread as bool> ",
                "<allow_early_temrmination as bool> "
            ));
            anyhow::bail!("Invalid/missing argument '{s}'");
        }
    };

    info!("Running memtester with: {memtester_args:#?}");
    let mut memory = vec![0; mem_usize_count];
    let report_list = Memtester::all_tests_random_order(memtester_args).run(&mut memory)?;
    info!("Tester ran for {:?}", start_time.elapsed());
    info!("Test results: \n{report_list}");

    if report_list.all_pass() {
        Ok(())
    } else {
        Err(anyhow!("Found failures or errors among memtest reports"))
    }
}

/// Parse the iter and return a usize for the requested memory vector length and other memtester argumentes
fn parse_args<I, S>(mut iter: I) -> Result<(usize, MemtesterArgs), &'static str>
where
    I: Iterator<Item = S>,
    S: AsRef<str>,
{
    const KB: usize = 1024;
    const MB: usize = 1024 * KB;

    macro_rules! parse_next(($n: literal) => {
        iter.next().and_then(|s| s.as_ref().parse().ok()).ok_or($n)?
    });

    let memsize: usize = parse_next!("memsize");
    let mem_usize_count = memsize * MB / size_of::<usize>();
    let timeout = Duration::from_millis(parse_next!("timeout_ms"));

    Ok((
        mem_usize_count,
        MemtesterArgs {
            timeout,
            require_memlock: parse_next!("require_memloc"),
            allow_working_set_resize: parse_next!("allow_working_set_resize"),
            allow_mem_resize: parse_next!("allow_mem_resize"),
            allow_multithread: parse_next!("allow_multithread"),
            allow_early_termination: parse_next!("allow_early_termination"),
        },
    ))
}
