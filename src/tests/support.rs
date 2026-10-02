// SPDX-License-Identifier: MIT
//! Shared fixtures for tests: a scripted [`Host`], a scripted HTTP server and
//! scratch directories. Nothing here is compiled outside `cargo test`.
use super::*;
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Commands and files served from memory. A key with several queued outputs
/// returns them in order and then keeps returning the last one, so a test can
/// script counters that advance between samples.
#[derive(Clone, Default)]
pub(crate) struct FakeHost {
    state: Arc<Mutex<FakeState>>,
}

#[derive(Default)]
struct FakeState {
    commands: HashMap<String, VecDeque<String>>,
    files: HashMap<PathBuf, VecDeque<String>>,
    dirs: HashMap<PathBuf, Vec<PathBuf>>,
    calls: Vec<String>,
    panic_on: Option<String>,
}

fn next(queue: &mut VecDeque<String>) -> Option<String> {
    if queue.len() > 1 {
        queue.pop_front()
    } else {
        queue.front().cloned()
    }
}

pub(crate) fn command_key(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

impl FakeHost {
    pub(crate) fn command(self, key: &str, output: &str) -> Self {
        self.state
            .lock()
            .unwrap()
            .commands
            .entry(key.into())
            .or_default()
            .push_back(output.into());
        self
    }

    pub(crate) fn file(self, path: &str, text: &str) -> Self {
        self.state
            .lock()
            .unwrap()
            .files
            .entry(PathBuf::from(path))
            .or_default()
            .push_back(text.into());
        self
    }

    pub(crate) fn dir(self, path: &str, entries: &[&str]) -> Self {
        self.state.lock().unwrap().dirs.insert(
            PathBuf::from(path),
            entries.iter().map(PathBuf::from).collect(),
        );
        self
    }

    /// Panic when this command runs, to exercise the sampler's isolation.
    pub(crate) fn panic_on(self, key: &str) -> Self {
        self.state.lock().unwrap().panic_on = Some(key.into());
        self
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }
}

impl Host for FakeHost {
    fn command(&self, program: &str, args: &[&str]) -> Option<String> {
        let key = command_key(program, args);
        let mut state = self.state.lock().unwrap();
        state.calls.push(key.clone());
        if state.panic_on.as_deref() == Some(key.as_str()) {
            drop(state);
            panic!("scripted host failure");
        }
        state.commands.get_mut(&key).and_then(next)
    }

    fn read_file(&self, path: &Path) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(format!("read {}", path.display()));
        state.files.get_mut(path).and_then(next)
    }

    fn read_dir(&self, path: &Path) -> Vec<PathBuf> {
        let state = self.state.lock().unwrap();
        state.dirs.get(path).cloned().unwrap_or_default()
    }
}

/// A unique, empty scratch directory removed when dropped.
pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = env::temp_dir().join(format!(
            "mlxtop-test-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One scripted exchange: the expected request line prefix and the reply.
pub(crate) struct Exchange {
    pub(crate) request: &'static str,
    pub(crate) status: u16,
    pub(crate) headers: &'static str,
    pub(crate) body: String,
}

pub(crate) fn reply(request: &'static str, status: u16, body: &str) -> Exchange {
    Exchange {
        request,
        status,
        headers: "",
        body: body.into(),
    }
}

/// Serve `script` in order on a loopback port and return every raw request.
pub(crate) fn serve(script: Vec<Exchange>) -> (u16, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for exchange in script {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = [0; 8192];
            let n = stream.read(&mut bytes).unwrap();
            let request = String::from_utf8_lossy(&bytes[..n]).into_owned();
            assert!(
                request.starts_with(exchange.request),
                "expected {:?}, got {request:?}",
                exchange.request
            );
            requests.push(request);
            write!(
                stream,
                "HTTP/1.1 {} Test\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
                exchange.status,
                exchange.body.len(),
                exchange.headers,
                exchange.body
            )
            .unwrap();
        }
        requests
    });
    (port, handle)
}

pub(crate) const MACOS_VM_STAT_1: &str =
    "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
Pages free:                               10000.\n\
Pages speculative:                         2000.\n\
Pages wired down:                         50000.\n\
Pages occupied by compressor:             10000.\n\
Pages stored in compressor:               40000.\n\
Anonymous pages:                         300000.\n\
File-backed pages:                       100000.\n\
Pages reactivated:                          100.\n\
Compressions:                              1000.\n\
Decompressions:                             500.\n\
Swapins:                                      0.\n\
Swapouts:                                     0.\n";

pub(crate) const MACOS_VM_STAT_2: &str =
    "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
Pages free:                                9000.\n\
Pages speculative:                         2000.\n\
Pages wired down:                         50000.\n\
Pages occupied by compressor:             10000.\n\
Pages stored in compressor:               40000.\n\
Anonymous pages:                         300000.\n\
File-backed pages:                       100000.\n\
Pages reactivated:                          200.\n\
Compressions:                              5000.\n\
Decompressions:                             900.\n\
Swapins:                                   2000.\n\
Swapouts:                                  4000.\n";

pub(crate) const IOREG_GPU: &str = r#"+-o AGXAcceleratorG14X  <class AGXAcceleratorG14X>
    "model" = "Apple M3 Max"
    "gpu-core-count" = 40
    "PerformanceStatistics" = {"In use system memory"=2147483648,"Alloc system memory"=4294967296,"Device Utilization %"=93,"Renderer Utilization %"=91,"Tiler Utilization %"=12}
"#;

pub(crate) const MACOS_PS: &str = "\
  101 15728640 85.5 40.0 R 120 omlx-server /opt/omlx/bin/omlx-server --port 8080\n\
  202 204800 1.0 0.5 S 3 Safari /Applications/Safari.app/Contents/MacOS/Safari\n";

/// A macOS host with a healthy oMLX process and paging counters that move
/// between the first and second sample.
pub(crate) fn macos_host() -> FakeHost {
    FakeHost::default()
        .command("/usr/sbin/sysctl -n hw.memsize", "38654705664\n")
        .command("/usr/sbin/sysctl -n hw.machine", "arm64\n")
        .command("/usr/sbin/sysctl -n iogpu.wired_limit_mb", "28000\n")
        .command("/usr/sbin/sysctl -n hw.pagesize", "16384\n")
        .command("/usr/sbin/ioreg -r -d 1 -w 0 -c IOAccelerator", IOREG_GPU)
        .command(
            "/usr/sbin/sysctl -n kern.memorystatus_vm_pressure_level",
            "1\n",
        )
        .command(
            "/usr/sbin/sysctl -n kern.memorystatus_vm_pressure_level",
            "2\n",
        )
        .command(
            "/usr/bin/memory_pressure -Q",
            "The system has 38654705664 (2359296 pages with a page size of 16384).\n\
             System-wide memory free percentage: 63%\n",
        )
        .command("/usr/bin/vm_stat", MACOS_VM_STAT_1)
        .command("/usr/bin/vm_stat", MACOS_VM_STAT_1)
        .command("/usr/bin/vm_stat", MACOS_VM_STAT_2)
        .command(
            "/usr/sbin/sysctl -n vm.swapusage",
            "total = 2048.00M  used = 512.00M  free = 1536.00M  (encrypted)\n",
        )
        .command(
            "/usr/bin/pmset -g therm",
            "Note: No thermal warning level has been recorded\n",
        )
        .command(
            "/bin/ps -axo pid=,rss=,%cpu=,%mem=,state=,pagein=,comm=,args=",
            MACOS_PS,
        )
        .command("/bin/date +%H:%M:%S", "12:00:01\n")
        .command("/bin/date +%H:%M:%S", "12:00:02\n")
}

pub(crate) const LINUX_MEMINFO: &str = "MemTotal:       16000000 kB\n\
MemFree:         2000000 kB\n\
MemAvailable:    4000000 kB\n\
Buffers:          100000 kB\n\
Cached:          3000000 kB\n\
Active(anon):    6000000 kB\n\
Inactive(anon):  1000000 kB\n\
Active(file):    1500000 kB\n\
Inactive(file):  1000000 kB\n\
SwapTotal:       4000000 kB\n\
SwapFree:        3000000 kB\n";

/// A Linux host with one NVIDIA card, an Ollama server and swap traffic
/// between the first and second sample.
pub(crate) fn linux_host() -> FakeHost {
    FakeHost::default()
        .file("/proc/meminfo", LINUX_MEMINFO)
        .command("getconf PAGESIZE", "4096\n")
        .command("uname -m", "x86_64\n")
        .file("/proc/vmstat", "pswpin 100\npswpout 200\n")
        .file("/proc/vmstat", "pswpin 1100\npswpout 2200\n")
        .file(
            "/proc/pressure/memory",
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=1\n\
             full avg10=0.50 avg60=0.00 avg300=0.00 total=1\n",
        )
        .command(
            "nvidia-smi --query-gpu=index,uuid,name,utilization.gpu,memory.used,memory.total,temperature.gpu --format=csv,noheader,nounits",
            "0, GPU-aaaa, NVIDIA RTX 4090, 64, 12000, 24564, 61\n",
        )
        .dir(
            "/sys/class/thermal",
            &["/sys/class/thermal/thermal_zone0", "/sys/class/thermal/cooling_device0"],
        )
        .file("/sys/class/thermal/thermal_zone0/temp", "72000\n")
        .command(
            "ps -axo pid=,rss=,%cpu=,%mem=,stat=,maj_flt=,comm=,args=",
            "  77 8388608 150.0 50.0 Sl 9 ollama /usr/local/bin/ollama serve\n",
        )
        .command("/bin/date +%H:%M:%S", "08:00:00\n")
        .command("/bin/date +%H:%M:%S", "08:00:01\n")
}

/// A config that points the oMLX client at a port nothing listens on.
pub(crate) fn offline_config() -> Config {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    Config {
        omx: Some(OmxConfig {
            host: Some("127.0.0.1".into()),
            port: Some(port),
        }),
        ..Config::default()
    }
}

pub(crate) fn empty_view() -> CollectorView {
    CollectorView {
        current: Sample::default(),
        generation_history: VecDeque::new(),
        prefill_history: VecDeque::new(),
        cache_history: VecDeque::new(),
        load_history: VecDeque::new(),
        swap_history: VecDeque::new(),
        gpu_history: VecDeque::new(),
        signals: VecDeque::new(),
        request_history: request_dashboard::History::default(),
        operator_history: operator_charts::History::default(),
    }
}
