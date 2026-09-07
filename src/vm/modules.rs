//! Built-in modules: `sys`, `os`, and `os.path`. Each is a [`Value::Module`]
//! whose members are native builtins or data. Only these built-ins are
//! importable in this build; user-module imports are a later task.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::value::{Builtin, Module, OroDict, Value};

/// Build the module named `name`, or `None` if it is not a built-in module.
/// `argv` seeds `sys.argv`.
pub fn build(name: &str, argv: &[String]) -> Option<Value> {
    match name {
        "sys" => Some(build_sys(argv)),
        "os" => Some(build_os()),
        _ => None,
    }
}

fn module(name: &str, members: Vec<(&str, Value)>) -> Value {
    let mut map = HashMap::new();
    for (k, v) in members {
        map.insert(k.to_string(), v);
    }
    Value::Module(Rc::new(Module { name: Rc::from(name), members: RefCell::new(map) }))
}

fn builtin(name: &'static str, func: fn(Vec<Value>) -> Result<Value, String>) -> Value {
    Value::Builtin(Rc::new(Builtin { name, func }))
}

fn build_sys(argv: &[String]) -> Value {
    let argv_list: Vec<Value> = argv.iter().map(|s| Value::str(s.clone())).collect();
    module(
        "sys",
        vec![
            ("argv", Value::List(Rc::new(RefCell::new(argv_list)))),
            ("exit", builtin("sys.exit", sys_exit)),
            ("platform", Value::str("oro")),
            ("stdout", Value::str("<stdout>")),
            ("stderr", Value::str("<stderr>")),
            ("stdin", Value::str("<stdin>")),
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
    Ok(Value::List(Rc::new(RefCell::new(names))))
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
    Ok(Value::Tuple(Rc::new(vec![Value::str(root), Value::str(ext)])))
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
