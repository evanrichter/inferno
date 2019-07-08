use std::collections::VecDeque;
use std::io::{self, prelude::*, BufRead};
use std::mem;

use crossbeam::channel;

use crate::collapse::{self, Collapse, Occurrences};

const TIDY_GENERIC: bool = true;
const TIDY_JAVA: bool = true;

mod logging {
    use log::{info, warn};

    pub(super) fn filtering_for_events_of_type(ty: &str) {
        info!("Filtering for events of type: {}", ty);
    }

    pub(super) fn weird_event_line(line: &str) {
        warn!("Weird event line: {}", line);
    }

    pub(super) fn weird_stack_line(line: &str) {
        warn!("Weird stack line: {}", line);
    }
}

/// Perf folder configuration options.
#[derive(Clone, Debug)]
pub struct Options {
    /// Annotate JIT functions with a `_[j]` suffix. Default is `false`.
    pub annotate_jit: bool,

    /// Annotate kernel functions with a `_[k]` suffix. Default is `false`.
    pub annotate_kernel: bool,

    /// Only consider samples of the given event type (see `perf list`). If this option is
    /// set to `None`, it will be set to the first encountered event type. Default is `None`.
    pub event_filter: Option<String>,

    /// Include raw addresses (e.g., `0xbfff0836`) where symbols can't be found.
    /// Default is `false`.
    pub include_addrs: bool,

    /// Include PID in the root frame. If disabled, the root frame is given the name of the
    /// profiled process. Default is `false`.
    pub include_pid: bool,

    /// Include TID and PID in the root frame. Implies `include_pid`. Default is `false`.
    pub include_tid: bool,

    /// The number of threads to use. Default is the number of logical cores on your machine.
    pub nthreads: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            annotate_jit: false,
            annotate_kernel: false,
            event_filter: None,
            include_addrs: false,
            include_pid: false,
            include_tid: false,
            nthreads: *collapse::DEFAULT_NTHREADS,
        }
    }
}

/// A stack collapser for the output of `perf script`.
///
/// To construct one, either use `perf::Folder::default()` or create an [`Options`] and use
/// `perf::Folder::from(options)`.
pub struct Folder {
    // State...
    /// General String cache that can be used while processing lines. Currently only used to keep
    /// track of functions for Java inlining.
    cache_line: Vec<String>,

    /// Similar to, but different from, the `event_filter` field on `Options`
    ///
    /// * Field on `Options` represents user's provided configuration and will never change.
    /// * This field, on the other hand, although initially set to the user's provided
    ///   configuration, represents state that may change as data is processed. In particular, if
    ///   the user provided `None` for their initial configuration, this field **will** change to
    ///   `Some(<event type>)` when we encounter the first event during processing. Merging together
    ///   different event types, such as instructions and cycles, would produce misleading results.
    event_filter: Option<String>,

    /// All lines until the next empty line are stack lines.
    in_event: bool,

    /// Number of stacks in each job sent to the threadpool.
    nstacks_per_job: usize,

    /// Number of times each call stack has been seen.
    occurrences: Occurrences,

    /// Current comm name.
    ///
    /// Called pname after original stackcollapse-perf source.
    pname: String,

    /// Skip all stack lines in this event.
    skip_stack: bool,

    /// Function entries on the stack in this entry thus far.
    stack: VecDeque<String>,

    // Options...
    opt: Options,
}

impl From<Options> for Folder {
    fn from(mut opt: Options) -> Self {
        if opt.nthreads == 0 {
            opt.nthreads = 1;
        }
        opt.include_pid = opt.include_pid || opt.include_tid;
        Self {
            cache_line: Vec::default(),
            event_filter: opt.event_filter.clone(),
            in_event: false,
            nstacks_per_job: collapse::NSTACKS_PER_JOB,
            occurrences: Occurrences::new(opt.nthreads),
            pname: String::default(),
            skip_stack: false,
            stack: VecDeque::default(),
            opt,
        }
    }
}

impl Default for Folder {
    fn default() -> Self {
        Options::default().into()
    }
}

impl Collapse for Folder {
    fn collapse<R, W>(&mut self, reader: R, writer: W) -> io::Result<()>
    where
        R: BufRead,
        W: Write,
    {
        // Do collapsing...
        if self.occurrences.is_concurrent() {
            self.collapse_multi_threaded(reader)?;
        } else {
            self.collapse_single_threaded(reader)?;
        }

        // Write results...
        self.occurrences.write_and_clear(writer)?;

        // Reset state...
        self.in_event = false;
        self.pname.clear();
        self.skip_stack = false;
        self.stack.clear();

        Ok(())
    }

    // Check if the input has an event line followed by a stack line.
    fn is_applicable(&mut self, input: &str) -> Option<bool> {
        let mut last_line_was_event_line = false;
        let mut input = input.as_bytes();
        let mut line = String::new();
        loop {
            line.clear();
            if let Ok(n) = input.read_line(&mut line) {
                if n == 0 {
                    break;
                }
            } else {
                return Some(false);
            }

            let line = line.trim();
            // Skip comments
            if line.starts_with('#') {
                continue;
            }

            if line.is_empty() {
                last_line_was_event_line = false;
                continue;
            }

            if last_line_was_event_line {
                // If this is valid input this line should be a stack line.
                return Some(Self::stack_line_parts(line).is_some());
            } else {
                if Self::event_line_parts(line).is_none() {
                    // The first line that's not empty or a comment should be an event line.
                    return Some(false);
                }
                last_line_was_event_line = true;
            }
        }
        None
    }

    #[cfg(test)]
    fn set_nstacks_per_job(&mut self, n: usize) {
        self.nstacks_per_job = n;
    }

    #[cfg(test)]
    fn set_nthreads(&mut self, n: usize) {
        self.opt.nthreads = n;
        self.occurrences = Occurrences::new(n);
    }
}

impl Folder {
    fn collapse_single_threaded<R>(&mut self, mut reader: R) -> io::Result<()>
    where
        R: BufRead,
    {
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if line.starts_with('#') {
                continue;
            }
            let line = line.trim_end();
            if line.is_empty() {
                self.after_event();
            } else if self.in_event {
                self.on_stack_line(line);
            } else {
                self.on_event_line(line);
            }
        }
    }

    fn collapse_multi_threaded<R>(&mut self, mut reader: R) -> io::Result<()>
    where
        R: io::BufRead,
    {
        // Invariants of this function for crate's authors to keep in mind...
        assert_ne!(self.nstacks_per_job, 0);
        assert!(self.occurrences.is_concurrent());
        assert!(self.opt.nthreads > 1);

        crossbeam::thread::scope(|scope| {
            // A `Folder` must always have an event filter, yet the user can choose `None` for
            // his/her event filter option. When the user chooses `None`, this means "set the event
            // filter to the first event we come across in the data." In a multithreaded context,
            // the consequences of this are, when the user has chosen `None` for the event filter
            // option, the initial state of each Folder on each thread needs information that can
            // only be known once we've started looking over the input data. They must wait on this;
            // therefore we set up a channel to be able to communicate what the event filter state
            // should be as soon as we know it and make the first thing each thread does be waiting
            // on receiving information from this channel.
            let (tx_event_filter, rx_event_filter) = channel::bounded(self.opt.nthreads);

            // If we know what the event filter state should be without having to look at the data
            // (because the user provided it), send this information down the channel immediately.
            if self.event_filter.is_some() {
                for _ in 0..self.opt.nthreads {
                    tx_event_filter.send(self.event_filter.clone()).unwrap();
                }
            }

            // Spin up the threadpool / worker threads.
            let (tx_input, rx_input) = channel::bounded::<Option<Vec<u8>>>(2 * self.opt.nthreads);
            let (tx_output, rx_output) = channel::bounded::<io::Result<()>>(1);
            let mut handles = Vec::with_capacity(self.opt.nthreads);
            for _ in 0..self.opt.nthreads {
                let rx_event_filter = rx_event_filter.clone();
                let rx_input = rx_input.clone();
                let tx_output = tx_output.clone();

                let nstacks_per_job = self.nstacks_per_job;
                let occurrences = self.occurrences.clone();
                let opt = self.opt.clone();

                let handle = scope.spawn(move |_| {
                    // Don't do anything until we know what the event filter should be
                    let event_filter = rx_event_filter.recv().unwrap();
                    let mut folder = Folder {
                        cache_line: Vec::default(),
                        event_filter,
                        in_event: false,
                        nstacks_per_job,
                        occurrences,
                        pname: String::default(),
                        skip_stack: false,
                        stack: VecDeque::default(),
                        opt,
                    };
                    while let Some(data) = rx_input.recv().unwrap() {
                        if let Err(e) = folder.collapse_single_threaded(&data[..]) {
                            // Use `try_send` here because we only need to send one
                            // error back to the main thread for propagation (even if
                            // multiple threads fail). If multiple threads fail,
                            // the first one to do so will fill up our output channel
                            // (which only has one slot); so plain `send` would block
                            // here, which can't happen because we need each thread to
                            // continuously pull values off the input channel or else
                            // it may fill up and clog. This is also why we continue
                            // after trying to send the error - to ensure each thread
                            // is always pulling values off the bounded input channel.
                            let _ = tx_output.try_send(Err(e));
                            continue;
                        }
                        folder.in_event = false;
                        folder.pname.clear();
                        folder.skip_stack = false;
                        folder.stack.clear();
                    }
                });
                handles.push(handle);
            }

            // On the main thread, start running over the input data in order to figure out how to
            // chunk it up to be sent to the worker threads. After having seen `self.nstacks_per_job`
            // number of stacks, send a slice of the input data that includes those stacks off to
            // the threadpool.
            //
            // In addition, if the user didn't provide an event filter, send the event filter to the
            // worker threads as soon as we learn what it is.

            // State for the loop...
            let buf_capacity =
                usize::next_power_of_two(collapse::NBYTES_PER_STACK_GUESS * self.nstacks_per_job);
            let mut buf = Vec::with_capacity(buf_capacity);
            let (mut index, mut nstacks) = (0, 0);

            // The loop...
            loop {
                let n = reader.read_until(b'\n', &mut buf)?;

                // If we're at the end of the data...
                if n == 0 {
                    // Send the last slice and exit the loop.
                    tx_input.send(Some(buf)).unwrap();
                    break;
                }

                let line = &buf[index..index + n];
                index += n;

                // If we've reached the end of a stack...
                if line.iter().all(|b| (*b as char).is_whitespace()) {
                    // Count it
                    nstacks += 1;
                    // If we've seen enough stacks to make up a slice...
                    if nstacks == self.nstacks_per_job {
                        // Send it.
                        let buf_capacity = buf.capacity();
                        let chunk = mem::replace(&mut buf, Vec::with_capacity(buf_capacity));
                        tx_input.send(Some(chunk)).unwrap();
                        // Reset the state; mark the beginning of the next slice.
                        index = 0;
                        nstacks = 0;
                    }
                    continue;
                }

                // If we don't know the event filter (because the user didn't provide it) look for
                // it; if we find it, send it to the threads, which need it to get started.
                if self.event_filter.is_none() {
                    let line = std::str::from_utf8(line)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                        .trim_end();
                    if let Some(event) = line.rsplitn(2, ' ').next() {
                        if event.ends_with(':') {
                            let event = &event[..(event.len() - 1)];
                            logging::filtering_for_events_of_type(event);
                            self.event_filter = Some(event.to_string());
                            for _ in 0..self.opt.nthreads {
                                tx_event_filter.send(self.event_filter.clone()).unwrap();
                            }
                        }
                    }
                }
            }

            // Send shutdown signal.
            for _ in &handles {
                tx_input.send(None).unwrap();
            }

            // Retrieve error, if any.
            drop(tx_output);
            if let Ok(result) = rx_output.recv() {
                result?;
            }

            // Join threads.
            for handle in handles {
                handle.join().unwrap();
            }

            Ok(())
        })
        .unwrap()
    }

    fn event_line_parts(line: &str) -> Option<(&str, &str, &str)> {
        let mut word_start = 0;
        let mut all_digits = false;
        let mut last_was_space = false;
        let mut contains_slash_at = None;
        for (idx, c) in line.char_indices() {
            if c == ' ' {
                if all_digits && !last_was_space {
                    // found an all-digit word
                    let (pid, tid) = if let Some(slash) = contains_slash_at {
                        // found PID + TID
                        (&line[word_start..slash], &line[(slash + 1)..idx])
                    } else {
                        // found TID
                        ("?", &line[word_start..idx])
                    };
                    // also trim comm in case multiple spaces were used to separate
                    let comm = line[..(word_start - 1)].trim();
                    return Some((comm, pid, tid));
                }
                word_start = idx + 1;
                all_digits = true;
            } else if c == '/' {
                if all_digits {
                    contains_slash_at = Some(idx);
                }
            } else if c.is_ascii_digit() {
                // we're still all digits if we were all digits
            } else {
                all_digits = false;
                contains_slash_at = None;
            }
            last_was_space = c == ' ';
        }
        None
    }

    // we have an event line, like:
    //
    //     java 25607 4794564.109216: cycles:
    //     java 12688 [002] 6544038.708352: cpu-clock:
    //     V8 WorkerThread 25607 4794564.109216: cycles:
    //     java 24636/25607 [000] 4794564.109216: cycles:
    //     java 12688/12764 6544038.708352: cpu-clock:
    //     V8 WorkerThread 24636/25607 [000] 94564.109216: cycles:
    //     vote   913    72.176760:     257597 cycles:uppp:
    fn on_event_line(&mut self, line: &str) {
        self.in_event = true;

        if let Some((comm, pid, tid)) = Self::event_line_parts(line) {
            if let Some(event) = line.rsplitn(2, ' ').next() {
                if event.ends_with(':') {
                    let event = &event[..(event.len() - 1)];

                    if let Some(ref event_filter) = self.event_filter {
                        if event != event_filter {
                            self.skip_stack = true;
                            return;
                        }
                    } else {
                        // By default only show events of the first encountered event type.
                        // Merging together different types, such as instructions and cycles,
                        // produces misleading results.
                        logging::filtering_for_events_of_type(event);
                        self.event_filter = Some(event.to_string());
                    }
                }
            }

            // XXX: re-use existing memory in pname if possible
            self.pname = comm.replace(' ', "_");
            if self.opt.include_tid {
                self.pname.push_str("-");
                self.pname.push_str(pid);
                self.pname.push_str("/");
                self.pname.push_str(tid);
            } else if self.opt.include_pid {
                self.pname.push_str("-");
                self.pname.push_str(pid);
            }
        } else {
            logging::weird_event_line(line);
            self.in_event = false;
        }
    }

    fn stack_line_parts(line: &str) -> Option<(&str, &str, &str)> {
        let mut line = line.trim_start().splitn(2, ' ');
        let pc = line.next()?.trim_end();
        let mut line = line.next()?.rsplitn(2, ' ');
        let mut module = line.next()?;
        // module is always wrapped in (), so remove those
        module = &module[1..(module.len() - 1)];
        let rawfunc = match line.next()?.trim() {
            // Sometimes there are two spaces betwen the pc and the (, like:
            //     7f1e2215d058  (/lib/x86_64-linux-gnu/libc-2.15.so)
            // In order to match the perl version, the rawfunc should be " ", and not "".
            "" => " ",
            s => s,
        };
        Some((pc, rawfunc, module))
    }

    // we have a stack line that shows one stack entry from the preceeding event, like:
    //
    //     ffffffff8103ce3b native_safe_halt ([kernel.kallsyms])
    //     ffffffff8101c6a3 default_idle ([kernel.kallsyms])
    //     ffffffff81013236 cpu_idle ([kernel.kallsyms])
    //     ffffffff815bf03e rest_init ([kernel.kallsyms])
    //     ffffffff81aebbfe start_kernel ([kernel.kallsyms].init.text)
    //     7f533952bc77 _dl_check_map_versions+0x597 (/usr/lib/ld-2.28.so)
    //     7f53389994d0 [unknown] ([unknown])
    //                0 [unknown] ([unknown])
    fn on_stack_line(&mut self, line: &str) {
        if self.skip_stack {
            return;
        }

        if let Some((pc, mut rawfunc, module)) = Self::stack_line_parts(line) {
            // Strip off symbol offsets
            if let Some(offset) = rawfunc.rfind("+0x") {
                let end = &rawfunc[(offset + 3)..];
                if end.chars().all(|c| char::is_ascii_hexdigit(&c)) {
                    // it's a symbol offset!
                    rawfunc = &rawfunc[..offset];
                }
            }

            // skip process names?
            // see https://github.com/brendangregg/FlameGraph/blob/f857ebc94bfe2a9bfdc4f1536ebacfb7466f69ba/stackcollapse-perf.pl#L269
            if rawfunc.starts_with('(') {
                return;
            }

            // Support Java inlining by splitting on "->". After the first func, the rest are
            // annotated with "_[i]" to mark them as inlined.
            // See https://github.com/brendangregg/FlameGraph/pull/89.
            for func in rawfunc.split("->") {
                let mut func = with_module_fallback(module, func, pc, self.opt.include_addrs);
                if TIDY_GENERIC {
                    func = tidy_generic(func);
                }

                if TIDY_JAVA && self.pname == "java" {
                    func = tidy_java(func);
                }

                // Annotations
                //
                // detect inlined when self.cache_line has funcs
                // detect kernel from the module name; eg, frames to parse include:
                //
                //     ffffffff8103ce3b native_safe_halt ([kernel.kallsyms])
                //     8c3453 tcp_sendmsg (/lib/modules/4.3.0-rc1-virtual/build/vmlinux)
                //     7d8 ipv4_conntrack_local+0x7f8f80b8 ([nf_conntrack_ipv4])
                //
                // detect jit from the module name; eg:
                //
                //     7f722d142778 Ljava/io/PrintStream;::print (/tmp/perf-19982.map)
                if !self.cache_line.is_empty() {
                    func.push_str("_[i]"); // inlined
                } else if self.opt.annotate_kernel
                    && (module.starts_with('[') || module.ends_with("vmlinux"))
                    && module != "[unknown]"
                {
                    func.push_str("_[k]"); // kernel
                } else if self.opt.annotate_jit
                    && module.starts_with("/tmp/perf-")
                    && module.ends_with(".map")
                {
                    func.push_str("_[j]"); // jitted
                }

                self.cache_line.push(func);
            }

            while let Some(func) = self.cache_line.pop() {
                self.stack.push_front(func);
            }
        } else {
            logging::weird_stack_line(line);
        }
    }

    fn after_event(&mut self) {
        // end of stack, so emit stack entry
        if !self.skip_stack {
            // allocate a string that is long enough to hold the entire stack string
            let mut stack_str = String::with_capacity(
                self.pname.len() + self.stack.iter().fold(0, |a, s| a + s.len() + 1),
            );

            // add the comm name
            stack_str.push_str(&self.pname);
            // add the other stack entries (if any)
            for e in self.stack.drain(..) {
                stack_str.push_str(";");
                stack_str.push_str(&e);
            }

            // count it!
            self.occurrences.add(stack_str, 1);
        }

        // reset for the next event
        self.in_event = false;
        self.skip_stack = false;
        self.stack.clear();
    }
}

// massage function name to be nicer
// NOTE: ignoring https://github.com/jvm-profiling-tools/perf-map-agent/pull/35
fn with_module_fallback(module: &str, func: &str, pc: &str, include_addrs: bool) -> String {
    if func != "[unknown]" {
        return func.to_string();
    }

    // try to use part of module name as function if unknown
    let func = match (module, include_addrs) {
        ("[unknown]", true) => "unknown",
        ("[unknown]", false) => {
            // no need to process this further
            return func.to_string();
        }
        (module, _) => {
            // use everything following last / of module as function name
            &module[module.rfind('/').map(|i| i + 1).unwrap_or(0)..]
        }
    };

    // output string is a bit longer than rawfunc but not much
    let mut res = String::with_capacity(func.len() + 12);

    if include_addrs {
        res.push_str("[");
        res.push_str(func);
        res.push_str(" <");
        res.push_str(pc);
        res.push_str(">]");
    } else {
        res.push_str("[");
        res.push_str(func);
        res.push_str("]");
    }

    res
}

fn tidy_generic(mut func: String) -> String {
    func = func.replace(';', ":");
    // remove argument list from function name, but _don't_ remove:
    //
    //  - Go method names like "net/http.(*Client).Do".
    //    see https://github.com/brendangregg/FlameGraph/pull/72
    //  - C++ anonymous namespace annotations.
    //    see https://github.com/brendangregg/FlameGraph/pull/93
    if let Some(first_paren) = func.find('(') {
        if func[first_paren..].starts_with("anonymous namespace)") {
            // C++ anonymous namespace
        } else {
            let mut is_go = false;
            if let Some(c) = func.get((first_paren - 1)..first_paren) {
                // if .get(-1) is None, can't be a dot
                if c == "." {
                    // assume it's a Go method name, so do nothing
                    is_go = true;
                }
            }

            if !is_go {
                // kill it with fire!
                func.truncate(first_paren);
            }
        }
    }

    // The perl version here strips ' and "; we don't do that.
    // see https://github.com/brendangregg/FlameGraph/commit/817c6ea3b92417349605e5715fe6a7cb8cbc9776
    func
}

fn tidy_java(mut func: String) -> String {
    // along with tidy_generic converts the following:
    //     Lorg/mozilla/javascript/ContextFactory;.call(Lorg/mozilla/javascript/ContextAction;)Ljava/lang/Object;
    //     Lorg/mozilla/javascript/ContextFactory;.call(Lorg/mozilla/javascript/C
    //     Lorg/mozilla/javascript/MemberBox;.<init>(Ljava/lang/reflect/Method;)V
    // into:
    //     org/mozilla/javascript/ContextFactory:.call
    //     org/mozilla/javascript/ContextFactory:.call
    //     org/mozilla/javascript/MemberBox:.init
    if func.starts_with('L') && func.contains('/') {
        func.remove(0);
    }

    func
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use lazy_static::lazy_static;
    use pretty_assertions::assert_eq;
    use rand::{Rng, SeedableRng};

    use super::*;
    use crate::collapse::tests_common;

    lazy_static! {
        static ref INPUT: Vec<PathBuf> = {
            [
                "./flamegraph/example-perf-stacks.txt.gz",
                "./flamegraph/test/perf-cycles-instructions-01.txt",
                "./flamegraph/test/perf-dd-stacks-01.txt",
                "./flamegraph/test/perf-funcab-cmd-01.txt",
                "./flamegraph/test/perf-funcab-pid-01.txt",
                "./flamegraph/test/perf-iperf-stacks-pidtid-01.txt",
                "./flamegraph/test/perf-java-faults-01.txt",
                "./flamegraph/test/perf-java-stacks-01.txt",
                "./flamegraph/test/perf-java-stacks-02.txt",
                "./flamegraph/test/perf-js-stacks-01.txt",
                "./flamegraph/test/perf-mirageos-stacks-01.txt",
                "./flamegraph/test/perf-numa-stacks-01.txt",
                "./flamegraph/test/perf-rust-Yamakaky-dcpu.txt",
                "./flamegraph/test/perf-vertx-stacks-01.txt",
                "./tests/data/collapse-perf/empty-line.txt",
                "./tests/data/collapse-perf/go-stacks.txt",
                "./tests/data/collapse-perf/java-inline.txt",
                "./tests/data/collapse-perf/weird-stack-line.txt",
            ]
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>()
        };
    }

    #[test]
    fn test_collapse_multi_perf() -> io::Result<()> {
        let mut folder = Folder::default();
        tests_common::test_collapse_multi(&mut folder, &INPUT)
    }

    #[test]
    #[ignore]
    fn test_collapse_multi_perf_simple() -> io::Result<()> {
        let path = "./flamegraph/test/perf-cycles-instructions-01.txt";
        let mut file = fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let mut folder = Folder::default();
        folder.collapse(&bytes[..], io::sink())
    }

    /// Varies the nstacks_per_job parameter and outputs the 10 fastests configurations by file.
    ///
    /// Command: `cargo test bench_nstacks_perf --release -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_nstacks_perf() -> io::Result<()> {
        let mut folder = Folder::default();
        tests_common::bench_nstacks(&mut folder, &INPUT)
    }

    #[test]
    #[ignore]
    /// Fuzz test the multithreaded collapser.
    ///
    /// Command: `cargo test fuzz_collapse_perf --release -- --ignored --nocapture`
    fn fuzz_collapse_perf() -> io::Result<()> {
        let seed = rand::thread_rng().gen::<u64>();
        println!("Random seed: {}", seed);
        let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);

        let mut buf_actual = Vec::new();
        let mut buf_expected = Vec::new();
        let mut count = 0;

        let inputs = tests_common::read_inputs(&INPUT)?;

        loop {
            let nstacks_per_job = rng.gen_range(1, 500 + 1);
            let options = Options {
                annotate_jit: rng.gen(),
                annotate_kernel: rng.gen(),
                event_filter: None,
                include_addrs: rng.gen(),
                include_pid: rng.gen(),
                include_tid: rng.gen(),
                nthreads: rng.gen_range(2, 32 + 1),
            };

            for (path, input) in inputs.iter() {
                buf_actual.clear();
                buf_expected.clear();

                let mut folder = {
                    let mut options = options.clone();
                    options.nthreads = 1;
                    Folder::from(options)
                };
                folder.nstacks_per_job = nstacks_per_job;
                folder.collapse(&input[..], &mut buf_expected)?;
                let expected = std::str::from_utf8(&buf_expected[..]).unwrap();

                let mut folder = Folder::from(options.clone());
                folder.nstacks_per_job = nstacks_per_job;
                folder.collapse(&input[..], &mut buf_actual)?;
                let actual = std::str::from_utf8(&buf_actual[..]).unwrap();

                if actual != expected {
                    eprintln!(
                        "Failed on file: {}\noptions: {:#?}\n",
                        path.display(),
                        options
                    );
                    assert_eq!(actual, expected);
                }
            }

            count += 1;
            if count % 10 == 0 {
                println!("Successfully ran {} fuzz tests.", count);
            }
        }
    }

}
