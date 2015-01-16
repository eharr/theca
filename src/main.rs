#![allow(unstable)]

extern crate libc;
extern crate time;
extern crate docopt;
extern crate "rustc-serialize" as rustc_serialize;
extern crate regex;
extern crate crypto;
extern crate term;

// std lib imports...
use std::os::{getenv, homedir};
use std::error;
use std::io::fs;
use std::io::fs::PathExtensions;
use std::io::process::{InheritFd};
use std::io::{File, Truncate, Write, Read, Open, ReadWrite,
              TempDir, Command, SeekSet, stdin, USER_RWX, IoError};
use std::iter::{repeat};
use std::string::FromUtf8Error;

// random things
use regex::{Regex};
use rustc_serialize::{Encodable, Decodable, Encoder, json};
use time::{now_utc, strftime, get_time, ParseError};
use docopt::Docopt;
use term::attr::Attr::{Bold};

// crypto imports
use crypt::{encrypt, decrypt, password_to_key};
use crypto::symmetriccipher::SymmetricCipherError;

pub use self::ErrorKind::{
    InternalIoError,
    GenericError
};

pub use self::libc::{
    STDIN_FILENO,
    STDOUT_FILENO,
    STDERR_FILENO
};

pub mod crypt;

static VERSION:  &'static str = "0.4.5-dev";

// c calls for TIOCGWINSZ
mod c {
    extern crate libc;
    pub use self::libc::{
        c_int,
        c_ushort,
        c_ulong,
        STDOUT_FILENO
    };
    use std::mem::zeroed;
    pub struct Winsize {
        pub ws_row: c_ushort,
        pub ws_col: c_ushort
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    static TIOCGWINSZ: c_ulong = 0x5413;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    static TIOCGWINSZ: c_ulong = 0x40087468;
    extern {
        pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }
    pub unsafe fn dimensions() -> Winsize {
        let mut window: Winsize = zeroed();
        ioctl(STDOUT_FILENO, TIOCGWINSZ, &mut window as *mut Winsize);
        window
    }
}

// unsafety wrapper
fn termsize() -> usize {
    let ws = unsafe {c::dimensions()};
    if ws.ws_col == 0 || ws.ws_row == 0 {
        0
    }
    else {
        ws.ws_col as usize
    }
}

static USAGE: &'static str = "
theca - cli note taking tool

Usage:
    theca [options] info
    theca [options] new-profile <name>
    theca [options] [-c] [-l LIMIT] [--reverse]
    theca [options] [-c] <id>
    theca [options] [-c] search [--body] [-l LIMIT] [--reverse] <pattern>
    theca [options] add <title> [--started|--urgent] [-b BODY|--editor|-]
    theca [options] edit <id> [<title>] [--started|--urgent|--none] [-b BODY|--editor|-]
    theca [options] del <id>
    theca (-h | --help)
    theca --version

Options:
    -h, --help                          Show this screen.
    -v, --version                       Show the version of theca.
    --profiles-folder PROFILEPATH       Path to folder container profile.json files.
    -p PROFILE, --profile PROFILE       Specify non-default profile.
    -c, --condensed                     Use the condensed print format.
    --encrypted                         Specifies using an encrypted profile.
    -k KEY, --key KEY                   Encryption key to use for encryption/decryption,
                                        a prompt will be displayed if no key is provided.
    -l LIMIT                            Limit listing to LIMIT items [default: 0].
    --none                              No status. (default)
    --started                           Started status.
    --urgent                            Urgent status.
    -b BODY                             Set body of the item from BODY.
    --editor                            Drop to $EDITOR to set/edit item body.
    -                                   Set body of the item from STDIN.
";

#[derive(RustcDecodable, Show)]
struct Args {
    flag_profiles_folder: String,
    flag_p: String,
    cmd_new_profile: bool,
    cmd_search: bool,
    flag_body: bool,
    flag_reverse: bool,
    cmd_add: bool,
    cmd_edit: bool,
    cmd_del: bool,
    arg_name: String,
    arg_pattern: String,
    flag_encrypted: bool,
    flag_key: String,
    flag_c: bool,
    flag_l: usize,
    arg_title: String,
    flag_started: bool,
    flag_urgent: bool,
    flag_none: bool,
    flag_b: String,
    flag_editor: bool,
    cmd__: bool,
    arg_id: Vec<usize>,
    flag_h: bool,
    flag_v: bool,
    cmd_info: bool
}

static NOSTATUS: &'static str = "";
static STARTED: &'static str = "Started";
static URGENT: &'static str = "Urgent";

#[derive(Copy)]
pub struct LineFormat {
    colsep: usize,
    id_width: usize,
    title_width: usize,
    status_width: usize,
    touched_width: usize
}

impl LineFormat {
    fn new(items: &Vec<ThecaItem>, args: &Args) -> Result<LineFormat, ThecaError> {
        // get termsize :>
        let console_width = termsize();

        // set colsep
        let colsep = match args.flag_c {
            true => 1,
            false => 2
        };

        let mut line_format = LineFormat {colsep: colsep, id_width:0, title_width:0,
                                          status_width:0, touched_width:0};

        // get length of longest id string
        line_format.id_width = match items.iter().max_by(|n| n.id.to_string()) {
            Some(w) => w.id.to_string().len(),
            None => 0
        };
        // if longest id is 1 char and we are using extended printing
        // then set id_width to 2 so "id" isn't truncated
        if line_format.id_width < 2 && !args.flag_c {line_format.id_width = 2;}

        // get length of longest title string
        line_format.title_width = match items.iter().max_by(|n| n.title.len()) {
            Some(w) => {
                if items.iter().any(|n| n.body.len() > 0) {
                    w.title.len()+4
                } else {
                    w.title.len()
                }
            },
            None => 0
        };
        // if using extended and longest title is less than 5 chars
        // set title_width to 5 so "title" won't be truncated
        if line_format.title_width < 5 && !args.flag_c {line_format.title_width = 5;}

        // sstatus length stuff
        line_format.status_width = match items.iter().any(|n| n.status.len() > 0) {
            true => {
                match args.flag_c {
                    // expanded print, get longest status (7 or 6 / started or urgent)
                    false => {
                        match items.iter().max_by(|n| n.status.len()) {
                            Some(w) => w.status.len(),
                            None => 0
                        }
                    },
                    // only display first char of status (e.g. S or U) for condensed print
                    true => 1
                }
            },
            // no items have statuses so truncate column
            false => 0
        };

        // last_touched has fixed string length so no need for silly iter stuff
        line_format.touched_width = match args.flag_c {
            true => 10, // condensed
            false => 19 // expanded
        };

        // check to make sure our new line format isn't bigger than the console
        let line_width = line_format.line_width();
        if console_width > 0 && line_width > console_width &&
           (line_format.title_width-(line_width-console_width)) > 0 {
            // if it is trim text from the title width since it is always the biggest...
            line_format.title_width -= line_width - console_width;
        }

        Ok(line_format)
    }

    fn line_width(&self) -> usize {
        self.id_width+self.title_width+self.status_width+self.touched_width+(3*self.colsep)
    }
}

#[derive(RustcDecodable, RustcEncodable, Clone)]
pub struct ThecaItem {
    id: usize,
    title: String,
    status: String,
    body: String,
    last_touched: String
}

impl ThecaItem {
    fn print(&self, line_format: &LineFormat, args: &Args) {
        let column_seperator: String = repeat(' ').take(line_format.colsep).collect();
        print!("{}", format_field(&self.id.to_string(), line_format.id_width, false));
        print!("{}", column_seperator);
        let mut title_str = self.title.to_string();
        if !self.body.is_empty() && !args.flag_body {
            title_str = "(+) ".to_string()+title_str.as_slice();
        }
        print!("{}", format_field(&title_str, line_format.title_width, true));
        print!("{}", column_seperator);
        print!("{}", format_field(&self.status, line_format.status_width, false));
        print!("{}", column_seperator);
        print!("{}", format_field(&self.last_touched, line_format.touched_width, false));
        print!("\n");
    }
}

#[derive(RustcDecodable, RustcEncodable)]
pub struct ThecaProfile {
    encrypted: bool,
    notes: Vec<ThecaItem>
}

pub enum ErrorKind {
    InternalIoError(IoError),
    GenericError
}

pub struct ThecaError {
    pub kind: ErrorKind,
    pub desc: String,
    pub detail: Option<String>
}

impl error::FromError<IoError> for ThecaError {
    fn from_error(err: IoError) -> ThecaError {
        ThecaError {
            kind: InternalIoError(err),
            desc: "An internal IO error ocurred.".to_string(),
            detail: None
        }
    }
}

impl error::FromError<(ErrorKind, &'static str)> for ThecaError {
    fn from_error((kind, desc): (ErrorKind, &'static str)) -> ThecaError {
        ThecaError { kind: kind, desc: desc.to_string(), detail: None }
    }
}

impl error::FromError<ParseError> for ThecaError {
    fn from_error(err: ParseError) -> ThecaError {
        ThecaError { kind: GenericError, desc: "Time parsing error".to_string(), detail: Some(err.to_string()) }
    }
}

impl error::FromError<FromUtf8Error> for ThecaError {
    fn from_error(err: FromUtf8Error) -> ThecaError {
        ThecaError { kind: GenericError, desc: "Error parsing invalid UTF8 characters.".to_string(), detail: Some(err.to_string()) }
    }
}

impl error::FromError<SymmetricCipherError> for ThecaError {
    fn from_error(err: SymmetricCipherError) -> ThecaError {
        ThecaError { kind: GenericError, desc: "SymmetricCipherError, that's bad.".to_string(), detail: None }
    }
}

impl error::FromError<docopt::Error> for ThecaError {
    fn from_error(err: docopt::Error) -> ThecaError {
        ThecaError { kind: GenericError, desc: err.to_string(), detail: None }
    }
}

macro_rules! generic_fail {
    ($short:expr) => ({
        return Err(::std::error::FromError::from_error(
            ThecaError {
                kind: GenericError,
                desc: $short,
                detail: None
            }
        ))
    })
}

impl ThecaProfile {
    fn new(args: &Args) -> Result<ThecaProfile, ThecaError> {
        if args.cmd_new_profile {
            let profile_path = find_profile_folder(args);
            // if the folder doesn't exist, make it yo!
            if !profile_path.exists() {
                try!(fs::mkdir(&profile_path, USER_RWX));
            }
            Ok(ThecaProfile {
                encrypted: args.flag_encrypted,
                notes: vec![]
            })
        } else {
            // set profile folder
            let mut profile_path = find_profile_folder(args);

            // set profile name
            if !args.flag_p.is_empty() {
                profile_path.push(args.flag_p.to_string() + ".json");
            } else {
                profile_path.push("default".to_string() + ".json");
            }

            // attempt to read profile
            match profile_path.is_file() {
                false => {
                    if profile_path.exists() {
                        generic_fail!(format!("{} is not a file.", profile_path.display()));
                    } else {
                        generic_fail!(format!("{} does not exist.", profile_path.display()));
                    }
                }
                true => {
                    if args.cmd_info {
                        println!("# Loading {}", profile_path.display());
                    }
                    let mut file = try!(File::open_mode(&profile_path, Open, Read));
                    let contents_buf = try!(file.read_to_end());
                    // decrypt the file if flag_encrypted
                    let contents = match args.flag_encrypted {
                        false => try!(String::from_utf8(contents_buf)),
                        true => {
                            let (key, iv) = password_to_key(args.flag_key.as_slice());
                                try!(String::from_utf8(try!(decrypt(
                                    contents_buf.as_slice(),
                                    key.as_slice(),
                                    iv.as_slice()
                                ))))
                        }
                    };
                    let decoded: ThecaProfile = match json::decode(contents.as_slice()) {
                        Ok(s) => s,
                        Err(_) => generic_fail!(format!("Invalid JSON in {}", profile_path.display()))
                    };
                    Ok(decoded)
                }
            }
        }
    }

    fn save_to_file(&mut self, args: &Args) -> Result<(), ThecaError> {
        // set profile folder
        let mut profile_path = find_profile_folder(args);

        // set file name
        // this needs some work.
        if !args.flag_p.is_empty() {
            profile_path.push(args.flag_p.to_string() + ".json");
        } else if args.cmd_new_profile && !args.arg_name.is_empty() {
            profile_path.push(args.arg_name.to_string() + ".json");
        } else {
            profile_path.push("default".to_string() + ".json");
        }

        // open file
        let mut file = try!(File::open_mode(&profile_path, Truncate, Write));

        // encode to buffer
        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut encoder = json::PrettyEncoder::new(&mut buffer);
            self.encode(&mut encoder).ok().expect("JSON encoding error.");
        }

        // encrypt json if its an encrypted profile
        if self.encrypted {
            let (key, iv) = password_to_key(args.flag_key.as_slice());
            buffer = try!(encrypt(
                buffer.as_slice(),
                key.as_slice(),
                iv.as_slice()
            ));
        }

        // write buffer to file
        file.write(buffer.as_slice())
            .ok()
            .expect(format!("Couldn't write to {}",profile_path.display()).as_slice());

        Ok(())
    }

    fn add_item(&mut self, args: &Args) -> Result<(), ThecaError> {
        let title = args.arg_title.replace("\n", "").to_string();
        let status = if args.flag_started {
            STARTED.to_string()
        } else if args.flag_urgent {
            URGENT.to_string()
        } else {
            NOSTATUS.to_string()
        };
        let body = if !args.flag_b.is_empty() {
            args.flag_b.to_string()
        } else if args.flag_editor {
            try!(drop_to_editor(&"".to_string()))
        } else if args.cmd__ {
            try!(stdin().lock().read_to_string())
        } else {
            "".to_string()
        };
        let new_id = match self.notes.last() {
            Some(n) => n.id,
            None => 0
        };
        self.notes.push(ThecaItem {
            id: new_id + 1,
            title: title,
            status: status,
            body: body,
            last_touched: try!(strftime("%F %T", &now_utc()))
        });
        println!("added");
        Ok(())
    }

    fn delete_item(&mut self, id: usize) {
        let remove = self.notes.iter()
            .position(|n| n.id == id)
            .map(|e| self.notes.remove(e))
            .is_some();
        match remove {
            true => {
                println!("removed");
            }
            false => {
                println!("not found");
            }
        }
    }

    fn edit_item(&mut self, id: usize, args: &Args) -> Result<(), ThecaError> {
        let item_pos: usize = match self.notes.iter()
                                              .position(|n| n.id == id) {
                Some(i) => i,
                None => generic_fail!(format!("note id#{} doesn't exist.", id))
            };
        if !args.arg_title.is_empty() {
            // change title
            self.notes[item_pos].title = args.arg_title.replace("\n", "").to_string();
        } else if args.flag_started || args.flag_urgent || args.flag_none {
            // change status
            if args.flag_started {
                self.notes[item_pos].status = STARTED.to_string();
            } else if args.flag_urgent {
                self.notes[item_pos].status = URGENT.to_string();
            } else if args.flag_none {
                self.notes[item_pos].status = NOSTATUS.to_string();
            }
        } else if !args.flag_b.is_empty() || args.flag_editor || args.cmd__ {
            // change body
            if !args.flag_b.is_empty() {
                self.notes[item_pos].body = args.flag_b.to_string();
            } else if args.flag_editor {
                self.notes[item_pos].body = try!(drop_to_editor(&self.notes[item_pos].body));
            } else if args.cmd__ {
                try!(stdin().lock().read_to_string());
            }
        }
        // update last_touched
        self.notes[item_pos].last_touched = try!(strftime("%F %T", &now_utc()));
        println!("edited");
        Ok(())
    }

    fn stats(&mut self) -> Result<(), ThecaError> {
        let no_s = self.notes.iter().filter(|n| n.status == "").count();
        let started_s = self.notes.iter().filter(|n| n.status == "Started").count();
        let urgent_s = self.notes.iter().filter(|n| n.status == "Urgent").count();
        let color = termsize() > 0;
        try!(pretty_line("encrypted: ", &format!("{}\n", self.encrypted), color));
        try!(pretty_line("notes: ", &format!("{}\n", self.notes.len()), color));
        try!(pretty_line("statuses: ", &format!("[none: {}, started: {}, urgent: {}]\n", no_s, started_s, urgent_s), color));
        Ok(())
    }

    fn print_header(&mut self, line_format: &LineFormat) -> Result<(), ThecaError> {
        let mut t = match term::stdout() {
            Some(t) => t,
            None => generic_fail!("Could not retrieve Standard Output.".to_string())
        };
        let column_seperator: String = repeat(' ').take(line_format.colsep).collect();
        let header_seperator: String = repeat('-').take(line_format.line_width()).collect();
        let color = termsize() > 0;
        if color {try!(t.attr(Bold));}
        try!(write!(
                    t, 
                    "{1}{0}{2}{0}{3}{0}{4}\n{5}\n",
                    column_seperator,
                    format_field(&"id".to_string(), line_format.id_width, false),
                    format_field(&"title".to_string(), line_format.title_width, false),
                    format_field(&"status".to_string(), line_format.status_width, false),
                    format_field(&"last touched".to_string(), line_format.touched_width, false),
                    header_seperator
                ));
        if color {try!(t.reset());}
        Ok(())
    }

    fn view_item(&mut self, id: usize, args: &Args) -> Result<(), ThecaError> {
        let note_pos = match self.notes.iter().position(|n| n.id == id) {
            Some(i) => i,
            None => generic_fail!(format!("Note #{} doesn't exist.", id))
        };
        let color = termsize() > 0;

        match args.flag_c {
            true => {
                try!(pretty_line("id: ", &format!("{}\n", self.notes[note_pos].id), color));
                try!(pretty_line("title: ", &format!("{}\n", self.notes[note_pos].title), color));
                if !self.notes[note_pos].status.is_empty() {
                    try!(pretty_line("status: ", &format!("{}\n", self.notes[note_pos].status), color));
                }
                try!(pretty_line(
                    "last touched: ",
                    &format!("{}\n", self.notes[note_pos].last_touched),
                    color
                ));
            },
            false => {
                try!(pretty_line("id\n--\n", &format!("{}\n\n", self.notes[note_pos].id), color));
                try!(pretty_line("title\n-----\n", &format!("{}\n\n", self.notes[note_pos].title), color));
                if !self.notes[note_pos].status.is_empty() {
                    try!(pretty_line(
                        "status\n------\n",
                        &format!("{}\n\n", self.notes[note_pos].status),
                        color
                    ));
                }
                try!(pretty_line(
                    "last touched\n------------\n",
                    &format!("{}\n\n", self.notes[note_pos].last_touched),
                    color
                ));
            }
        };

        // body
        if !self.notes[note_pos].body.is_empty() {
            match args.flag_c {
                true => {
                    try!(pretty_line("body: ", &format!("{}\n", self.notes[note_pos].body), color));
                },
                false => {
                    try!(pretty_line("body\n----\n", &format!("{}\n\n", self.notes[note_pos].body), color));
                }
            };
        }
        Ok(())
    }

    fn list_items(&mut self, args: &Args) -> Result<(), ThecaError> {
        if self.notes.len() > 0 {
            let line_format = try!(LineFormat::new(&self.notes, args));
            if !args.flag_c {
                try!(self.print_header(&line_format));
            }
            let list_range = if args.flag_l > 0 {
                args.flag_l
            } else {
                self.notes.len()
            };
            match args.flag_reverse {
                false => {
                    for i in range(0, list_range) {
                        self.notes[i].print(&line_format, args);
                    }
                }, true => {
                    for i in range(0, list_range).rev() {
                        self.notes[i].print(&line_format, args);
                    }
                }
            };
        }
        Ok(())
    }

    fn search_items(&mut self, regex_pattern: &str, args: &Args) -> Result<(), ThecaError> {
        let re = match Regex::new(regex_pattern) {
            Ok(r) => r,
            Err(e) => generic_fail!(format!("Regex error: {}", e))
        };
        let notes: Vec<ThecaItem> = match args.flag_body {
            true => self.notes.iter().filter(|n| re.is_match(n.body.as_slice()))
                              .map(|n| n.clone()).collect(),
            false => self.notes.iter().filter(|n| re.is_match(n.title.as_slice()))
                               .map(|n| n.clone()).collect()
        };
        let line_format = try!(LineFormat::new(&notes, args));
        if !args.flag_c {
            try!(self.print_header(&line_format));
        }
        match args.flag_reverse {
            false => {
                for i in range(0, notes.len()) {
                    notes[i].print(&line_format, args);
                    if args.flag_body {
                        // println!("\t{}", notes[i].body);
                        for l in notes[i].body.lines() {
                            println!("\t{}", l);
                        }
                    }
                }
            },
            true => {
                for i in range(0, notes.len()).rev() {
                    notes[i].print(&line_format, args);
                    if args.flag_body {
                        println!("\t{}", notes[i].body);
                    }
                }
            }
        };
        Ok(())
    }
}

fn pretty_line(bold: &str, plain: &String, color: bool) -> Result<(), ThecaError> {
    let mut t = match term::stdout() {
        Some(t) => t,
        None => generic_fail!("Could not retrieve Standard Output.".to_string())
    };
    if color {try!(t.attr(Bold));}
    try!(write!(t, "{}", bold.to_string()));
    if color {try!(t.reset());}
    try!(write!(t, "{}", plain));
    Ok(())
}

fn format_field(value: &String, width: usize, truncate: bool) -> String {
    if value.len() > width && width > 3 && truncate {
        format!("{: <1$.1$}...", value, width-3)
    } else {
        format!("{: <1$.1$}", value, width)
    }
}

fn find_profile_folder(args: &Args) -> Path {
    if !args.flag_profiles_folder.is_empty() {
        Path::new(args.flag_profiles_folder.to_string())
    } else {
        match homedir() {
            Some(ref p) => p.join(".theca"),
            None => Path::new(".").join(".theca")
        }
    }
}

fn drop_to_editor(contents: &String) -> Result<String, ThecaError> {
    // this could probably be prettyified tbh!

    // setup temporary directory
    let tmpdir = try!(TempDir::new("theca"));
    // let tmpdir = match TempDir::new("theca") {
    //     Ok(dir) => dir,
    //     Err(e) => generic_fail!(format!("couldn't create temporary directory: {}", e))
    // };
    // setup temporary file to write/read
    let tmppath = tmpdir.path().join(get_time().sec.to_string());
    let mut tmpfile = try!(File::open_mode(&tmppath, Open, ReadWrite));
    tmpfile.write_line(contents.as_slice()).ok().expect("Failed to write line to temp file");
    // we now have a temp file, at `tmppath`, that contains `contents`
    // first we need to know which onqe
    let editor = match getenv("VISUAL") {
        Some(val) => val,
        None => {
            match getenv("EDITOR") {
                Some(val) => val,
                None => generic_fail!("Neither $VISUAL nor $EDITOR is set.".to_string())
            }
        }
    };
    // lets start `editor` and edit the file at `tmppath`
    // first we need to set STDIN, STDOUT, and STDERR to those that theca is
    // currently using so we can display the editor
    let mut editor_command = Command::new(editor);
    editor_command.arg(tmppath.display().to_string());
    editor_command.stdin(InheritFd(STDIN_FILENO));
    editor_command.stdout(InheritFd(STDOUT_FILENO));
    editor_command.stderr(InheritFd(STDERR_FILENO));
    let editor_proc = editor_command.spawn();
    match editor_proc.ok().expect("Couldn't launch editor").wait().is_ok() {
        true => {
            // finished editing, time to read `tmpfile` for the final output
            // seek to start of `tmpfile`
            tmpfile.seek(0, SeekSet).ok().expect("Can't seek to start of temp file");
            Ok(try!(tmpfile.read_to_string()))
        }
        false => generic_fail!("The editor broke... I think".to_string())
    }
}

fn get_password() -> Result<String, ThecaError> {
    // should really turn off terminal echo...
    print!("Key: ");
    let mut stdin = std::io::stdio::stdin();
    // since this only reads one line of stdin it could still feasibly
    // be used with `-` to set note body?
    let key = try!(stdin.read_line());
    Ok(key.trim().to_string())
}

fn theca() -> Result<(), ThecaError> {
    let mut args: Args = try!(Docopt::new(USAGE)
                            .and_then(|d| d.decode()));

    match getenv("THECA_DEFAULT_PROFILE") {
        Some(val) => {
            if args.flag_p.is_empty() {
                args.flag_p = val;
            }
        },
        None => ()
    };
    match getenv("THECA_PROFILE_FOLDER") {
        Some(val) => {
            if args.flag_profiles_folder.is_empty() {
                args.flag_profiles_folder = val;
            }
        },
        None => ()
    };
    
    // if profile in ecnrypted try to set the key
    if args.flag_encrypted && args.flag_key.is_empty() {
        args.flag_key = try!(get_password());
    }

    let mut profile = try!(ThecaProfile::new(&args));
    // what root command was used
    if args.cmd_add {
        // add a item
        try!(profile.add_item(&args));
    } else if args.cmd_edit {
        // edit a item
        try!(profile.edit_item(args.arg_id[0], &args));
    } else if args.cmd_del {
        // delete a item
        profile.delete_item(args.arg_id[0]);
    } else if args.flag_v {
        // display theca version
        println!("theca v{}", VERSION);
    } else if args.cmd_search {
        // search for an item
        try!(profile.search_items(args.arg_pattern.as_slice(), &args));
    } else if !args.arg_id.is_empty() {
        // view short item
        try!(profile.view_item(args.arg_id[0], &args));
    } else if args.cmd_info {
        try!(profile.stats());
    } else if !args.cmd_new_profile {
        // this should be the default for nothing
        try!(profile.list_items(&args));
    }

    // save altered profile back to disk
    // this should only be triggered by commands that make transactions to the profile
    if args.cmd_add || args.cmd_edit || args.cmd_del || args.cmd_new_profile {
        try!(profile.save_to_file(&args));
    }
    Ok(())
}

fn main() {
    // is anything stored in the ENV?

    // wooo error unwinding yay
    match theca() {
        Err(e) => println!("{}", e.desc),
        Ok(_) => ()
    };
}
