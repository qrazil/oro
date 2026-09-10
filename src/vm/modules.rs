//! Built-in modules: `sys`, `os`, and `os.path`. Each is a [`Value::Module`]
//! whose members are native builtins or data. Only these built-ins are
//! importable in this build; user-module imports are a later task.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::value::{Builtin, Module, OroDict, OroList, OroTuple, Value};

/// Build the module named `name`, or `None` if it is not a built-in module.
/// `argv` seeds `sys.argv`.
pub fn build(name: &str, argv: &[String]) -> Option<Value> {
    match name {
        "sys" => Some(build_sys(argv)),
        "os" => Some(build_os()),
        "time" => Some(build_time()),
        "re" => Some(build_re()),
        "proc" => Some(build_proc()),
        "net" => Some(build_net()),
        "_io" => Some(build_native_io()),
        "_json" => Some(build_native_json()),
        _ => None,
    }
}

fn module(name: &str, members: Vec<(&str, Value)>) -> Value {
    let mut map = HashMap::new();
    for (k, v) in members {
        map.insert(Rc::from(k), v);
    }
    Value::Module(Rc::new(Module { name: Rc::from(name), members: RefCell::new(map) }))
}

fn builtin(name: &'static str, func: fn(Vec<Value>) -> Result<Value, String>) -> Value {
    Value::Builtin(Rc::new(Builtin { name, func }))
}

/// One of the three standard streams, shared process-wide.
///
/// `modules::build` runs on every `import`, and two readers on fd 0 would each
/// hold their own buffer — one of them able to swallow bytes the other is
/// waiting for. The three standard streams are identities, not values, so they
/// are made once.
fn std_stream(which: &'static str) -> Value {
    thread_local! {
        static STREAMS: RefCell<HashMap<&'static str, Value>> = RefCell::new(HashMap::new());
    }
    STREAMS.with(|s| {
        s.borrow_mut()
            .entry(which)
            .or_insert_with(|| Value::Stream(Rc::new(crate::stream::OroStream::std_stream(which))))
            .clone()
    })
}

fn build_sys(argv: &[String]) -> Value {
    let argv_list: Vec<Value> = argv.iter().map(|s| Value::str(s.clone())).collect();
    module(
        "sys",
        vec![
            ("argv", Value::List(OroList::new(argv_list))),
            ("exit", builtin("sys.exit", sys_exit)),
            ("platform", Value::str("oro")),
            // Real fd-backed byte streams, not name placeholders. They are
            // unbuffered like every other writer in the language, which is why
            // Oro flushes as it goes (`python3 -u` semantics) — there is
            // nothing to flush rather than something that flushes eagerly.
            // `print` writes through the same handle, so `print(...)` and
            // `sys.stdout.write(b"...")` interleave in program order.
            ("stdout", std_stream("<stdout>")),
            ("stderr", std_stream("<stderr>")),
            ("stdin", std_stream("<stdin>")),
        ],
    )
}

fn build_os() -> Value {
    let mut env = OroDict::new();
    for (k, v) in std::env::vars() {
        let _ = env.insert(Value::str(k), Value::str(v));
    }
    module(
        "os",
        vec![
            ("environ", Value::Dict(Rc::new(RefCell::new(env)))),
            ("getcwd", builtin("os.getcwd", os_getcwd)),
            ("listdir", builtin("os.listdir", os_listdir)),
            ("remove", builtin("os.remove", os_remove)),
            ("mkdir", builtin("os.mkdir", os_mkdir)),
            ("path", build_os_path()),
        ],
    )
}

fn build_os_path() -> Value {
    module(
        "os.path",
        vec![
            ("exists", builtin("os.path.exists", path_exists)),
            ("isfile", builtin("os.path.isfile", path_isfile)),
            ("isdir", builtin("os.path.isdir", path_isdir)),
            ("join", builtin("os.path.join", path_join)),
            ("basename", builtin("os.path.basename", path_basename)),
            ("dirname", builtin("os.path.dirname", path_dirname)),
            ("splitext", builtin("os.path.splitext", path_splitext)),
        ],
    )
}

// --- time --------------------------------------------------------------------

fn build_time() -> Value {
    module(
        "time",
        vec![
            ("time", builtin("time.time", time_time)),
            ("sleep", builtin("time.sleep", time_sleep)),
            ("monotonic", builtin("time.monotonic", time_monotonic)),
        ],
    )
}

/// Wall-clock epoch seconds. Can jump or go backwards (NTP, DST, manual clock
/// changes), so use it for *when*, never for measuring *how long*.
fn time_time(args: Vec<Value>) -> Result<Value, String> {
    if !args.is_empty() {
        return Err("time() takes no arguments".to_string());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "system clock is before the epoch".to_string())?;
    Ok(Value::Float(now.as_secs_f64()))
}

/// `time.sleep` is finished in the VM: it *parks* the calling task on the
/// reactor's deadline list rather than stopping the OS thread, so every other
/// task keeps running. A builtin can only answer with a `Value`, and the whole
/// content of this one is the `Step` it answers with, so this stub is never
/// invoked directly — the same arrangement `proc.run` has.
fn time_sleep(_args: Vec<Value>) -> Result<Value, String> {
    Err("internal: time.sleep must be dispatched by the VM (it parks)".to_string())
}

/// Monotonic seconds from a fixed reference — only ever increases. Use it for
/// *how long* (elapsed time, timeouts, benchmarks).
fn time_monotonic(args: Vec<Value>) -> Result<Value, String> {
    use std::sync::OnceLock;
    use std::time::Instant;
    static BASE: OnceLock<Instant> = OnceLock::new();
    if !args.is_empty() {
        return Err("monotonic() takes no arguments".to_string());
    }
    let base = BASE.get_or_init(Instant::now);
    Ok(Value::Float(base.elapsed().as_secs_f64()))
}

// --- re ----------------------------------------------------------------------

fn build_re() -> Value {
    module(
        "re",
        vec![
            ("search", builtin("re.search", re_search)),
            ("findall", builtin("re.findall", re_findall)),
            ("finditer", builtin("re.finditer", re_finditer)),
            ("fullmatch", builtin("re.fullmatch", re_fullmatch)),
            ("sub", builtin("re.sub", re_sub)),
            ("split", builtin("re.split", re_split)),
            ("compile", builtin("re.compile", re_compile)),
            ("match", builtin("re.match", re_match)),
        ],
    )
}

fn str_at(args: &[Value], i: usize, who: &str) -> Result<String, String> {
    match args.get(i) {
        Some(Value::Str(s)) => Ok(s.s.clone()),
        Some(other) => Err(format!("{who}() argument {} must be str, not '{}'", i + 1, other.type_name())),
        None => Err(format!("{who}() missing a required argument")),
    }
}

fn re_search(args: Vec<Value>) -> Result<Value, String> {
    let re = crate::regexutil::compile(&str_at(&args, 0, "search")?)?;
    Ok(crate::regexutil::search(&re, &str_at(&args, 1, "search")?))
}

fn re_findall(args: Vec<Value>) -> Result<Value, String> {
    let re = crate::regexutil::compile(&str_at(&args, 0, "findall")?)?;
    Ok(crate::regexutil::findall(&re, &str_at(&args, 1, "findall")?))
}

fn re_finditer(args: Vec<Value>) -> Result<Value, String> {
    let re = crate::regexutil::compile(&str_at(&args, 0, "finditer")?)?;
    Ok(crate::regexutil::finditer(&re, &str_at(&args, 1, "finditer")?))
}

fn re_fullmatch(args: Vec<Value>) -> Result<Value, String> {
    let re = crate::regexutil::compile(&str_at(&args, 0, "fullmatch")?)?;
    Ok(crate::regexutil::fullmatch(&re, &str_at(&args, 1, "fullmatch")?))
}

fn re_sub(args: Vec<Value>) -> Result<Value, String> {
    let re = crate::regexutil::compile(&str_at(&args, 0, "sub")?)?;
    let repl = str_at(&args, 1, "sub")?;
    Ok(crate::regexutil::sub(&re, &repl, &str_at(&args, 2, "sub")?))
}

fn re_split(args: Vec<Value>) -> Result<Value, String> {
    let re = crate::regexutil::compile(&str_at(&args, 0, "split")?)?;
    Ok(crate::regexutil::split(&re, &str_at(&args, 1, "split")?))
}

fn re_compile(args: Vec<Value>) -> Result<Value, String> {
    crate::regexutil::regex_value(&str_at(&args, 0, "compile")?)
}

/// `re.match` is deliberately cut — it anchors at the start, which is almost
/// always not what people mean.
fn re_match(_args: Vec<Value>) -> Result<Value, String> {
    Err("re.match is not supported in Oro — it anchors at the start of the string, which is \
         almost always the wrong choice and is constantly confused with re.search. Use re.search \
         (unanchored), or anchor explicitly with a leading `^`."
        .to_string())
}

// --- net ---------------------------------------------------------------------

/// Two constructors and two objects, and that is the whole module
/// (`docs/stdlib-server-design.md` §4). Everything else a socket can do is a
/// method on the stream it hands back, because a `TcpStream` is a Reader and a
/// Writer like every other stream in the language.
///
/// `net` is Rust rather than Oro because every line of it is a syscall — the
/// §5 rule, "anything that touches every byte goes in Rust". There is no
/// `_net` half and no `std/net.oro`: nothing here is policy.
fn build_net() -> Value {
    module(
        "net",
        vec![
            ("listen", builtin("net.listen", net_listen)),
            ("dial", builtin("net.dial", net_dial)),
        ],
    )
}

fn net_listen(args: Vec<Value>) -> Result<Value, String> {
    // `reuseport=True` is in §4's sketch and is not here: sharing one port
    // across N VMs is the scale-out story, and there is one VM.
    let addr = one_addr(&args, "listen")?;
    Ok(Value::Stream(Rc::new(crate::net::listen(&addr)?)))
}

/// `net.dial` is finished in the VM: it *parks* the calling task, first on the
/// system resolver and then on the handshake, and a `Builtin` can only answer
/// with a `Value`. This entry exists so the name resolves and is callable; the
/// dispatch in [`super::Vm::invoke`] takes it before it can ever run.
fn net_dial(_args: Vec<Value>) -> Result<Value, String> {
    Err("internal: net.dial must be dispatched by the VM (it parks)".to_string())
}

/// The one argument both constructors take: an address, as a string. No
/// `Address` type — see §4.
pub(super) fn one_addr(args: &[Value], who: &str) -> Result<String, String> {
    match args {
        [Value::Str(s)] => Ok(s.s.clone()),
        [other] => Err(format!(
            "{who}() address must be str, not '{}' — addresses are strings like \"127.0.0.1:8080\"",
            other.type_name()
        )),
        _ => Err(format!("{who}() takes exactly one address")),
    }
}

// --- _io ----------------------------------------------------------------------

/// The Rust primitives behind `std/io.oro`, and nothing else.
///
/// Two things in the `io` module cannot be written in Oro: the `Buffer`
/// constructor (it is a Rust type) and the whole-stream read (it `stat`s and
/// allocates once, and `bytes` is immutable so the Oro spelling would hold the
/// chunks and the joined result at the same time — the 2× transient the
/// optimisation exists to avoid). Everything else in `io` is Oro.
///
/// The leading underscore is enforced, not a convention: [`super::Vm`] resolves
/// an underscored built-in module only from inside a stdlib module body, so
/// this is not part of the language's surface and is not frozen at 1.0.
fn build_native_io() -> Value {
    module(
        "_io",
        vec![
            ("buffer", builtin("_io.buffer", io_buffer)),
            ("read_all", builtin("_io.read_all", io_read_all)),
        ],
    )
}

fn io_buffer(args: Vec<Value>) -> Result<Value, String> {
    match args.as_slice() {
        [Value::Bytes(b)] => Ok(Value::Stream(Rc::new(crate::stream::OroStream::buffer(
            (**b).clone(),
        )))),
        [other] => Err(format!("buffer() argument must be bytes, not '{}'", other.type_name())),
        _ => Err("buffer() takes 1 argument".to_string()),
    }
}

fn io_read_all(args: Vec<Value>) -> Result<Value, String> {
    match args.as_slice() {
        [Value::Stream(s)] => Ok(Value::bytes(s.read_all()?)),
        _ => Err("internal: _io.read_all takes one Rust stream".to_string()),
    }
}

// --- _json --------------------------------------------------------------------

/// The JSON codec behind `std/json.oro`.
///
/// The one place in the standard library where the Rust/Oro line moved *after*
/// it was drawn, and `docs/stdlib-server-design.md` §5 records why: a JSON
/// parser is a per-byte loop, which is precisely what §5's own rule sends to
/// Rust, and the Oro spelling measured 95x CPython's C `json` on a 1 KB
/// payload. `std/json.oro` keeps the module's surface, its documentation and
/// its defaults; this is the loop underneath.
///
/// Underscored, and so — like `_io` — resolvable only from inside a stdlib
/// module body. `json` is the language surface; `_json` is not, and is not
/// frozen at 1.0.
fn build_native_json() -> Value {
    module(
        "_json",
        vec![
            ("parse", builtin("_json.parse", json_parse)),
            ("stringify", builtin("_json.stringify", json_stringify)),
        ],
    )
}

fn json_parse(args: Vec<Value>) -> Result<Value, String> {
    match args.as_slice() {
        [Value::Str(s)] => crate::json::parse(&s.s),
        // `bytes` is not quietly decoded: §1's whole argument is that the
        // decode is a step the program takes, in the open — `b.to_str()` —
        // rather than something a codec guesses at.
        [other] => Err(format!(
            "parse() argument must be str, not '{}'",
            other.type_name()
        )),
        _ => Err("internal: _json.parse takes one string".to_string()),
    }
}

fn json_stringify(args: Vec<Value>) -> Result<Value, String> {
    match args.as_slice() {
        [value, indent] => {
            let indent = match indent {
                Value::None => None,
                other => Some(other),
            };
            crate::json::stringify(value, indent).map(Value::str)
        }
        _ => Err("internal: _json.stringify takes a value and an indent".to_string()),
    }
}

// --- proc ---------------------------------------------------------------------

fn build_proc() -> Value {
    // `run` is finished in the VM (it takes keyword args and builds a Completed);
    // this stub is never invoked directly.
    module("proc", vec![("run", builtin("proc.run", proc_run_stub))])
}

fn proc_run_stub(_args: Vec<Value>) -> Result<Value, String> {
    Err("internal: proc.run must be dispatched by the VM".to_string())
}

// --- sys ---------------------------------------------------------------------

/// Encodes an exit request as a sentinel error the VM turns into `SystemExit`.
fn sys_exit(args: Vec<Value>) -> Result<Value, String> {
    let code = match args.as_slice() {
        [] | [Value::None] => 0,
        [Value::Int(n)] => *n,
        [Value::Bool(b)] => *b as i64,
        _ => return Err("sys.exit() code must be an int or None in this build".to_string()),
    };
    Err(format!("\u{0}exit\u{0}{code}"))
}

// --- os ----------------------------------------------------------------------

fn os_getcwd(args: Vec<Value>) -> Result<Value, String> {
    if !args.is_empty() {
        return Err("getcwd() takes no arguments".to_string());
    }
    std::env::current_dir()
        .map(|p| Value::str(p.to_string_lossy().into_owned()))
        .map_err(|e| e.to_string())
}

fn os_listdir(args: Vec<Value>) -> Result<Value, String> {
    let path = one_path(&args, "listdir")?;
    let mut names = Vec::new();
    let entries = std::fs::read_dir(&path).map_err(|e| io_err(&e, &path))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        names.push(Value::str(entry.file_name().to_string_lossy().into_owned()));
    }
    Ok(Value::List(OroList::new(names)))
}

fn os_remove(args: Vec<Value>) -> Result<Value, String> {
    let path = one_path(&args, "remove")?;
    std::fs::remove_file(&path).map_err(|e| io_err(&e, &path))?;
    Ok(Value::None)
}

fn os_mkdir(args: Vec<Value>) -> Result<Value, String> {
    let path = one_path(&args, "mkdir")?;
    std::fs::create_dir(&path).map_err(|e| io_err(&e, &path))?;
    Ok(Value::None)
}

// --- os.path -----------------------------------------------------------------

fn path_exists(args: Vec<Value>) -> Result<Value, String> {
    let path = one_path(&args, "exists")?;
    Ok(Value::Bool(std::path::Path::new(&path).exists()))
}

fn path_isfile(args: Vec<Value>) -> Result<Value, String> {
    let path = one_path(&args, "isfile")?;
    Ok(Value::Bool(std::path::Path::new(&path).is_file()))
}

fn path_isdir(args: Vec<Value>) -> Result<Value, String> {
    let path = one_path(&args, "isdir")?;
    Ok(Value::Bool(std::path::Path::new(&path).is_dir()))
}

fn path_join(args: Vec<Value>) -> Result<Value, String> {
    // Follows POSIX join: an absolute later component resets the path.
    let mut out = String::new();
    for a in &args {
        let seg = as_str(a, "join")?;
        if seg.starts_with('/') || out.is_empty() {
            out = seg.to_string();
        } else if out.ends_with('/') {
            out.push_str(seg);
        } else {
            out.push('/');
            out.push_str(seg);
        }
    }
    Ok(Value::str(out))
}

fn path_basename(args: Vec<Value>) -> Result<Value, String> {
    let path = one_path(&args, "basename")?;
    let base = path.rsplit('/').next().unwrap_or("").to_string();
    Ok(Value::str(base))
}

fn path_dirname(args: Vec<Value>) -> Result<Value, String> {
    let path = one_path(&args, "dirname")?;
    match path.rfind('/') {
        Some(i) => Ok(Value::str(path[..i].to_string())),
        None => Ok(Value::str(String::new())),
    }
}

fn path_splitext(args: Vec<Value>) -> Result<Value, String> {
    let path = one_path(&args, "splitext")?;
    let base_start = path.rfind('/').map(|i| i + 1).unwrap_or(0);
    // A leading dot in the basename is not an extension (".bashrc").
    let dot = path[base_start..]
        .rfind('.')
        .map(|i| base_start + i)
        .filter(|&i| i > base_start);
    let (root, ext) = match dot {
        Some(i) => (path[..i].to_string(), path[i..].to_string()),
        None => (path.clone(), String::new()),
    };
    Ok(Value::Tuple(OroTuple::new(vec![Value::str(root), Value::str(ext)])))
}

// --- helpers -----------------------------------------------------------------

fn as_str<'a>(v: &'a Value, who: &str) -> Result<&'a str, String> {
    match v {
        Value::Str(s) => Ok(&s.s),
        other => Err(format!("{who}() argument must be str, not '{}'", other.type_name())),
    }
}

fn one_path(args: &[Value], who: &str) -> Result<String, String> {
    match args {
        [v] => Ok(as_str(v, who)?.to_string()),
        _ => Err(format!("{who}() takes exactly one argument")),
    }
}

/// Format a filesystem error like CPython so the VM classifies it into the
/// right exception type (`FileNotFoundError`, `PermissionError`, `OSError`).
pub fn io_err(e: &std::io::Error, path: &str) -> String {
    use std::io::ErrorKind::*;
    let (errno, msg) = match e.kind() {
        NotFound => (2, "No such file or directory"),
        PermissionDenied => (13, "Permission denied"),
        AlreadyExists => (17, "File exists"),
        _ => (0, "OS error"),
    };
    format!("[Errno {errno}] {msg}: '{path}'")
}
