mod list {
    pub mod converter;
    pub mod ranges;
}
mod impure {
    #[cfg(feature = "oniguruma")]
    pub mod onig;
}
mod pure {
    #[cfg(not(feature = "oniguruma"))]
    pub mod onig;
}
mod errors;
mod token;

#[macro_use]
extern crate lazy_static;

use log::debug;
use regex::Regex;
use std::env;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use structopt::StructOpt;
use token::Token;

#[cfg(feature = "oniguruma")]
use impure::onig;

#[cfg(not(feature = "oniguruma"))]
use pure::onig;

const CMD: &'static str = env!("CARGO_PKG_NAME"); // "teip"
pub const DEFAULT_CAP: usize = 1024;

pub fn msg_error(msg: &str) {
    eprintln!("{}: {}", CMD, msg);
}

pub fn error_exit(msg: &str) -> ! {
    msg_error(msg);
    std::process::exit(1);
}

/// Exit silently because the error can be intentional.
pub fn exit_silently(msg: &str) -> ! {
    debug!("SIGPIPE?:{}", msg);
    std::process::exit(1);
}

pub struct PipeIntercepter {
    tx: Sender<Token>,
    pipe_writer: BufWriter<Box<dyn Write + Send + 'static>>, // Not used when -s
    handler: Option<JoinHandle<()>>,                         // "option dance"
    line_end: u8,
    solid: bool,
    dryrun: bool,
}

impl PipeIntercepter {
    // Spawn another process which continuously prints results
    fn start_output(
        cmds: Vec<String>,
        line_end: u8,
        dryrun: bool,
    ) -> Result<PipeIntercepter, errors::SpawnError> {
        let (tx, rx) = mpsc::channel();
        let (child_stdin, child_stdout, _) = PipeIntercepter::exec_cmd(&cmds)?;
        let pipe_writer = BufWriter::new(child_stdin);
        let handler = thread::spawn(move || {
            debug!("thread: spawn");
            let mut reader = BufReader::new(child_stdout);
            let mut writer = BufWriter::new(io::stdout());
            loop {
                match rx.recv() {
                    Ok(token) => match token {
                        Token::Channel(msg) => {
                            debug!("thread: rx.recv <= Channle:[{}]", msg);
                            writer
                                .write(msg.as_bytes())
                                .unwrap_or_else(|e| exit_silently(&e.to_string()));
                        }
                        Token::Piped => {
                            debug!("thread: rx.recv <= Piped");
                            match PipeIntercepter::read_pipe(&mut reader, line_end) {
                                Ok(msg) => {
                                    writer
                                        .write(msg.as_bytes())
                                        .unwrap_or_else(|e| exit_silently(&e.to_string()));
                                }
                                Err(e) => {
                                    // pipe may be exhausted
                                    writer.flush().unwrap();
                                    error_exit(&e.to_string())
                                }
                            }
                        }
                        Token::EOF => {
                            debug!("thread: rx.recv <= EOF");
                            break;
                        }
                        _ => {
                            error_exit("Exit with bug.");
                        }
                    },
                    Err(e) => {
                        msg_error(&e.to_string());
                        break;
                    }
                }
            }
        });
        Ok(PipeIntercepter {
            tx: tx,
            pipe_writer: pipe_writer,
            handler: Some(handler),
            line_end: line_end,
            solid: false,
            dryrun: dryrun,
        })
    }

    // Spawn another process for solid mode
    fn start_solid_output(
        cmds: Vec<String>,
        line_end: u8,
        dryrun: bool,
    ) -> Result<PipeIntercepter, errors::SpawnError> {
        let (tx, rx) = mpsc::channel();
        let handler = thread::spawn(move || {
            debug!("thread: spawn");
            let mut writer = BufWriter::new(io::stdout());
            loop {
                match rx.recv() {
                    Ok(token) => match token {
                        Token::Channel(msg) => {
                            debug!("thread: rx.recv <= Channle:[{}]", msg);
                            writer
                                .write(msg.as_bytes())
                                .unwrap_or_else(|e| exit_silently(&e.to_string()));
                        }
                        Token::Solid(msg) => {
                            debug!("thread: rx.recv <= Solid:[{}]", msg);
                            let result = PipeIntercepter::exec_cmd_sync(msg, &cmds, line_end);
                            writer
                                .write(result.as_bytes())
                                .unwrap_or_else(|e| exit_silently(&e.to_string()));
                        }
                        Token::EOF => {
                            debug!("thread: rx.recv <= EOF");
                            break;
                        }
                        _ => {
                            error_exit("Exit with bug.");
                        }
                    },
                    Err(e) => {
                        msg_error(&e.to_string());
                        break;
                    }
                }
            }
        });
        let dummy = Box::new(io::sink());
        Ok(PipeIntercepter {
            tx: tx,
            pipe_writer: BufWriter::new(dummy),
            handler: Some(handler),
            line_end: line_end,
            solid: true,
            dryrun: dryrun,
        })
    }

    fn read_pipe<R: BufRead + ?Sized>(
        reader: &mut R,
        line_end: u8,
    ) -> Result<String, errors::PipeReceiveError> {
        debug!("thread: read_pipe");
        let mut buf = Vec::with_capacity(DEFAULT_CAP);
        let n = reader
            .read_until(line_end, &mut buf)
            .map_err(|e| errors::PipeReceiveError::Io(e))?;
        if n == 0 {
            // If pipe is exhausted, throw error.
            return Err(errors::PipeReceiveError::EndOfFd);
        }
        trim_eol(&mut buf);
        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    fn exec_cmd(
        cmds: &Vec<String>,
    ) -> Result<
        (
            Box<dyn Write + Send + 'static>,
            Box<dyn Read + Send + 'static>,
            String,
        ),
        errors::SpawnError,
    > {
        debug!("thread: exec_cmd: {:?}", cmds);
        if cmds.len() == 0 {
            // In the case of dryrun, return dummy objects.
            return Ok((Box::new(io::sink()), Box::new(io::empty()), "".to_string()));
        }
        let child = Command::new(&cmds[0])
            .args(&cmds[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| errors::SpawnError::Io(e))?;
        let first = &cmds[0];
        let child_stdin = child.stdin.ok_or(errors::SpawnError::StdinOpenFailed)?;
        let child_stdout = child.stdout.ok_or(errors::SpawnError::StdoutOpenFailed)?;
        Ok((
            Box::new(child_stdin),
            Box::new(child_stdout),
            first.to_string(),
        ))
    }

    fn exec_cmd_sync(input: String, cmds: &Vec<String>, line_end: u8) -> String {
        debug!("thread: exec_cmd_sync: {:?}", &cmds);
        let mut child = Command::new(&cmds[0])
            .args(&cmds[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("Failed to spawn child process");
        {
            let stdin = child.stdin.as_mut().expect("Failed to open stdin");
            let mut vec = Vec::new();
            vec.extend_from_slice(input.as_bytes());
            vec.extend_from_slice(&[line_end]);
            stdin
                .write_all(vec.as_slice())
                .expect("Failed to write to stdin");
        }
        let mut output = child
            .wait_with_output()
            .expect("Failed to read stdout")
            .stdout;
        if output.ends_with(&[line_end]) {
            output.pop();
        }
        String::from_utf8_lossy(&output).to_string()
    }

    fn send_msg(&self, msg: String) -> Result<(), errors::TokenSendError> {
        debug!("tx.send => Channle({})", msg);
        self.tx
            .send(Token::Channel(msg))
            .map_err(|e| errors::TokenSendError::Channel(e))?;
        Ok(())
    }

    fn send_pipe(&mut self, msg: String) -> Result<(), errors::TokenSendError> {
        if self.dryrun {
            let msg_annotated: String;
            msg_annotated = HL[0].to_string() + &msg + HL[1];
            debug!("tx.send => Channle({})", msg_annotated);
            self.tx
                .send(Token::Channel(msg_annotated))
                .map_err(|e| errors::TokenSendError::Channel(e))?;
            return Ok(());
        }
        if self.solid {
            debug!("tx.send => Solid({})", msg);
            self.tx
                .send(Token::Solid(msg))
                .map_err(|e| errors::TokenSendError::Channel(e))?;
            Ok(())
        } else {
            debug!("tx.send => Piped");
            self.tx
                .send(Token::Piped)
                .map_err(|e| errors::TokenSendError::Channel(e))?;
            debug!("stdin => {}[line_end]", msg);
            self.pipe_writer
                .write(msg.as_bytes())
                .map_err(|e| errors::TokenSendError::Pipe(e))?;
            self.pipe_writer
                .write(&[self.line_end])
                .map_err(|e| errors::TokenSendError::Pipe(e))?;
            Ok(())
        }
    }

    fn send_eof(&self) -> Result<(), errors::TokenSendError> {
        debug!("tx.send => EOF");
        self.tx
            .send(Token::EOF)
            .map_err(|e| errors::TokenSendError::Channel(e))?;
        Ok(())
    }
}

impl Drop for PipeIntercepter {
    fn drop(&mut self) {
        debug!("close pipe");
        // Replace the writer with a dummy object to close the pipe.
        self.pipe_writer = BufWriter::new(Box::new(io::sink()));
        self.handler.take().unwrap().join().unwrap();
    }
}

lazy_static! {
    static ref USAGE: String = format!(
        "
Allow the command handle selected parts of the standard input, and bypass other parts.

Usage:
  {cmd} -g <pattern> [-oGsvz] [--] [<command>...]
  {cmd} -f <list> [-d <delimiter> | -D <pattern>] [-svz] [--] [<command>...]
  {cmd} -c <list> [-svz] [--] [<command>...]
  {cmd} -l <list> [-svz] [--] [<command>...]
  {cmd} --help | --version

Options:
  --help          Display this help and exit
  --version       Show version and exit
  -g <pattern>    Select lines that match the regular expression <pattern>
  -o              -g selects only matched parts
  -G              -g adopts Oniguruma regular expressions
  -f <list>       Select only these white-space separated fields
  -d <delimiter>  Use <delimiter> for field delimiter of -f
  -D <pattern>    Use regular expression <pattern> for field delimiter of -f
  -c <list>       Select only these characters
  -l <list>       Select only these lines
  -s              Execute command for each selected part
  -v              Invert the sense of selecting
  -z              Line delimiter is NUL instead of newline
",
        cmd = CMD
    );
    static ref REGEX_WS: Regex = Regex::new("\\s+").unwrap();
    static ref DEFAULT_HIGHLIGHT: String = match env::var("TEIP_HIGHLIGHT") {
        Ok(v) => v,
        Err(_) => "\x1b[36m[\x1b[0m\x1b[01;31m{}\x1b[0m\x1b[36m]\x1b[0m".to_string(),
    };
    static ref HL: Vec<&'static str> = DEFAULT_HIGHLIGHT.split("{}").collect();
}

#[derive(StructOpt, Debug)]
#[structopt(
    author,
    about = "Allow the command handle selected parts of the standard input, and bypass other parts."
)]
struct Args {
    #[structopt(
        short = "g",
        name = "pattern",
        help = "Select lines that match the regular expression <pattern>"
    )]
    regex: Option<String>,
    #[structopt(short = "o", help = "-g selects only matched parts")]
    only_matched: bool,
    #[structopt(short = "G", help = "-g adopts Oniguruma regular expressions")]
    onig_enabled: bool,
    #[structopt(
        short = "f",
        name = "list",
        help = "Select only these white-space separated fields"
    )]
    list: Option<String>,
    #[structopt(short = "d", help = "Use <delimiter> for field delimiter of -f")]
    delimiter: Option<String>,
    #[structopt(
        short = "D",
        name = "delimiter pattern",
        help = "Use regular expression <pattern> for field delimiter of -f"
    )]
    regexp_delimiter: Option<String>,
    #[structopt(short = "c", name = "char list", help = "Select only these characters")]
    char: Option<String>,
    #[structopt(short = "l", name = "line list", help = "Select only these lines")]
    line: Option<String>,
    #[structopt(short = "s", help = "Execute command for each selected part")]
    solid: bool,
    #[structopt(short = "v", help = "Invert the sense of selecting")]
    invert: bool,
    #[structopt(short = "z", help = "Line delimiter is NUL instead of newline")]
    zero: bool,

    #[structopt(name = "command")]
    commands: Vec<String>,
}

fn main() {
    env_logger::init();

    // ***** Parse options and prepare configures *****
    let args: Args = Args::from_args();

    debug!("{:?}", args);

    if HL.len() < 2 {
        error_exit("Invalid format in TEIP_HIGHLIGHT variable")
    }

    let flag_zero = args.zero;
    let cmds: Vec<&str> = args.commands.iter().map(|s| s.as_str()).collect();
    let flag_only = args.only_matched;
    let flag_regex = args.regex.is_some();
    let flag_onig = args.onig_enabled;
    let flag_solid = args.solid;
    let flag_invert = args.invert;
    let flag_char = args.char.is_some();
    let flag_lines = args.line.is_some();
    let flag_field = args.list.is_some();
    let flag_delimiter = args.delimiter.is_some();
    let delimiter = args.delimiter.as_ref().map(|s| s.as_str()).unwrap_or("");
    let flag_regex_delimiter = args.regexp_delimiter.is_some();

    let mut regex_mode = String::new();
    let mut regex = Regex::new("").unwrap();
    let mut regex_onig = onig::new_regex();
    let mut line_end = b'\n';
    let mut single_token_per_line = false;
    let mut ch: PipeIntercepter;
    let mut flag_dryrun = true;
    let regex_delimiter;
    let char_list = args
        .char
        .as_ref()
        .and_then(|s| {
            list::converter::to_ranges(s.as_str(), flag_invert)
                .map_err(|e| error_exit(&e.to_string()))
                .ok()
        })
        .unwrap_or_else(|| list::converter::to_ranges("1", true).unwrap());

    let field_list = args
        .list
        .as_ref()
        .and_then(|s| {
            list::converter::to_ranges(s.as_str(), flag_invert)
                .map_err(|e| error_exit(&e.to_string()))
                .ok()
        })
        .unwrap_or_else(|| list::converter::to_ranges("1", true).unwrap());

    let line_list = args
        .line
        .as_ref()
        .and_then(|s| {
            list::converter::to_ranges(s.as_str(), flag_invert)
                .map_err(|e| error_exit(&e.to_string()))
                .ok()
        })
        .unwrap_or_else(|| list::converter::to_ranges("1", true).unwrap());

    if flag_zero {
        regex_mode = "(?ms)".to_string();
        line_end = b'\0';
    }

    if !flag_onig {
        regex =
            Regex::new(&(regex_mode.to_owned() + args.regex.as_ref().unwrap_or(&"".to_owned())))
                .unwrap_or_else(|e| error_exit(&e.to_string()));
    } else {
        if flag_zero {
            regex_onig =
                onig::new_option_multiline_regex(args.regex.as_ref().unwrap_or(&"".to_owned()));
        } else {
            regex_onig = onig::new_option_none_regex(args.regex.as_ref().unwrap_or(&"".to_owned()));
        }
    }

    if flag_regex_delimiter {
        regex_delimiter =
            Regex::new(&(regex_mode.to_string() + args.regexp_delimiter.as_ref().unwrap()))
                .unwrap_or_else(|e| error_exit(&e.to_string()));
    } else {
        regex_delimiter = REGEX_WS.clone();
    }

    if cmds.len() > 0 {
        flag_dryrun = false;
    }

    if (!flag_only && flag_regex) || flag_lines {
        single_token_per_line = true;
    }

    if flag_solid {
        ch =
            PipeIntercepter::start_solid_output(vecstr_rm_references(&cmds), line_end, flag_dryrun)
                .unwrap_or_else(|e| error_exit(&e.to_string()));
    } else {
        ch = PipeIntercepter::start_output(vecstr_rm_references(&cmds), line_end, flag_dryrun)
            .unwrap_or_else(|e| error_exit(&e.to_string()));
    }

    // ***** Start processing *****
    if single_token_per_line {
        if flag_lines {
            line_line_proc(&mut ch, &line_list, line_end)
                .unwrap_or_else(|e| error_exit(&e.to_string()));
        } else if flag_regex {
            if flag_onig {
                onig::regex_onig_line_proc(&mut ch, &regex_onig, flag_invert, line_end)
                    .unwrap_or_else(|e| error_exit(&e.to_string()));
            } else {
                regex_line_proc(&mut ch, &regex, flag_invert, line_end)
                    .unwrap_or_else(|e| error_exit(&e.to_string()));
            }
        }
    } else {
        let stdin = io::stdin();
        loop {
            let mut buf = Vec::with_capacity(DEFAULT_CAP);
            match stdin.lock().read_until(line_end, &mut buf) {
                Ok(n) => {
                    if n == 0 {
                        ch.send_eof().unwrap_or_else(|e| msg_error(&e.to_string()));
                        break;
                    }
                    let eol = trim_eol(&mut buf);
                    if flag_regex {
                        if flag_onig {
                            onig::regex_onig_proc(&mut ch, &buf, &regex_onig, flag_invert)
                                .unwrap_or_else(|e| error_exit(&e.to_string()));
                        } else {
                            regex_proc(&mut ch, &buf, &regex, flag_invert)
                                .unwrap_or_else(|e| error_exit(&e.to_string()));
                        }
                    } else if flag_char {
                        char_proc(&mut ch, &buf, &char_list)
                            .unwrap_or_else(|e| error_exit(&e.to_string()));
                    } else if flag_field && flag_delimiter {
                        field_proc(&mut ch, &buf, delimiter, &field_list)
                            .unwrap_or_else(|e| error_exit(&e.to_string()));
                    } else if flag_field {
                        field_regex_proc(&mut ch, &buf, &regex_delimiter, &field_list)
                            .unwrap_or_else(|e| error_exit(&e.to_string()));
                    }
                    ch.send_msg(eol)
                        .unwrap_or_else(|e| msg_error(&e.to_string()));
                }
                Err(e) => msg_error(&e.to_string()),
            }
        }
    }
}

fn line_line_proc(
    ch: &mut PipeIntercepter,
    ranges: &Vec<list::ranges::Range>,
    line_end: u8,
) -> Result<(), errors::TokenSendError> {
    let mut i: usize = 0;
    let mut ri: usize = 0;
    let stdin = io::stdin();
    loop {
        let mut buf = Vec::with_capacity(DEFAULT_CAP);
        match stdin.lock().read_until(line_end, &mut buf) {
            Ok(n) => {
                let eol = trim_eol(&mut buf);
                let line = String::from_utf8_lossy(&buf).to_string();
                if n == 0 {
                    ch.send_eof()?;
                    break;
                }
                if ranges[ri].high < (i + 1) && (ri + 1) < ranges.len() {
                    ri += 1;
                }
                if ranges[ri].low <= (i + 1) && (i + 1) <= ranges[ri].high {
                    ch.send_pipe(line.to_string())?;
                } else {
                    ch.send_msg(line.to_string())?;
                }
                ch.send_msg(eol)?;
            }
            Err(e) => msg_error(&e.to_string()),
        }
        i += 1;
    }
    Ok(())
}

fn regex_line_proc(
    ch: &mut PipeIntercepter,
    re: &Regex,
    invert: bool,
    line_end: u8,
) -> Result<(), errors::TokenSendError> {
    let stdin = io::stdin();
    loop {
        let mut buf = Vec::with_capacity(DEFAULT_CAP);
        match stdin.lock().read_until(line_end, &mut buf) {
            Ok(n) => {
                let eol = trim_eol(&mut buf);
                if n == 0 {
                    ch.send_eof()?;
                    break;
                }
                let line = String::from_utf8_lossy(&buf).to_string();
                if re.is_match(&line) {
                    if invert {
                        ch.send_msg(line.to_string())?;
                    } else {
                        ch.send_pipe(line.to_string())?;
                    }
                } else {
                    if invert {
                        ch.send_pipe(line.to_string())?;
                    } else {
                        ch.send_msg(line.to_string())?;
                    }
                }
                ch.send_msg(eol)?;
            }
            Err(e) => msg_error(&e.to_string()),
        }
    }
    Ok(())
}

/// Handles regex ( -g )
fn regex_proc(
    ch: &mut PipeIntercepter,
    line: &Vec<u8>,
    re: &Regex,
    invert: bool,
) -> Result<(), errors::TokenSendError> {
    let line = String::from_utf8_lossy(&line).to_string();
    let mut left_index = 0;
    let mut right_index;
    for cap in re.find_iter(&line) {
        right_index = cap.start();
        let unmatched = &line[left_index..right_index];
        let matched = &line[cap.start()..cap.end()];
        // Ignore empty string.
        // Regex "*" matches empty, but , in most situations,
        // handling empty string is not helpful for users.
        if !unmatched.is_empty() {
            if !invert {
                ch.send_msg(unmatched.to_string())?;
            } else {
                ch.send_pipe(unmatched.to_string())?;
            }
        }
        if !invert {
            ch.send_pipe(matched.to_string())?;
        } else {
            ch.send_msg(matched.to_string())?;
        }
        left_index = cap.end();
    }
    if left_index < line.len() {
        let unmatched = &line[left_index..line.len()];
        if !invert {
            ch.send_msg(unmatched.to_string())?;
        } else {
            ch.send_pipe(unmatched.to_string())?;
        }
    }
    Ok(())
}

/// Handles character range ( -c )
fn char_proc(
    ch: &mut PipeIntercepter,
    line: &Vec<u8>,
    ranges: &Vec<list::ranges::Range>,
) -> Result<(), errors::TokenSendError> {
    let line = String::from_utf8_lossy(&line).to_string();
    let cs = line.chars();
    let mut str_in = String::new();
    let mut str_out = String::new();
    let mut ri = 0;
    let mut is_in;
    let mut last_is_in = false;
    // Merge consequent characters' range to execute commands as few times as possible.
    for (i, c) in cs.enumerate() {
        if ranges[ri].high < (i + 1) && (ri + 1) < ranges.len() {
            ri += 1;
        }
        if ranges[ri].low <= (i + 1) && (i + 1) <= ranges[ri].high {
            is_in = true;
            str_in.push(c);
        } else {
            is_in = false;
            str_out.push(c);
        }
        if is_in && !last_is_in {
            ch.send_msg(str_out.to_string())?;
            str_out.clear();
        } else if !is_in && last_is_in {
            ch.send_pipe(str_in.to_string())?;
            str_in.clear();
        }
        last_is_in = is_in;
    }
    if last_is_in && !str_in.is_empty() {
        ch.send_pipe(str_in)?;
    } else {
        ch.send_msg(str_out)?;
    }
    Ok(())
}

/// Handles white space separation ( -f )
fn field_regex_proc(
    ch: &mut PipeIntercepter,
    line: &Vec<u8>,
    re: &Regex,
    ranges: &Vec<list::ranges::Range>,
) -> Result<(), errors::TokenSendError> {
    let line = String::from_utf8_lossy(&line).to_string();
    let mut i = 1; // current field index
    let mut ri = 0;
    let mut left_index = 0;
    let mut right_index;
    for cap in re.find_iter(&line) {
        right_index = cap.start();
        let field = &line[left_index..right_index]; // This can be empty string
        let spaces = &line[cap.start()..cap.end()];
        left_index = cap.end();
        if ranges[ri].high < i && (ri + 1) < ranges.len() {
            ri += 1;
        }
        if ranges[ri].low <= i && i <= ranges[ri].high {
            ch.send_pipe(field.to_string())?;
        } else {
            ch.send_msg(field.to_string())?;
        }
        ch.send_msg(spaces.to_string())?;
        i += 1;
    }
    // If line ends with delimiter, empty fields must be handled.
    if left_index <= line.len() {
        if ranges[ri].high < i && (ri + 1) < ranges.len() {
            ri += 1;
        }
        // filed is empty if line ends with delimiter
        let field = &line[left_index..line.len()];
        if ranges[ri].low <= i && i <= ranges[ri].high {
            ch.send_pipe(field.to_string())?;
        } else {
            ch.send_msg(field.to_string())?;
        }
    }
    Ok(())
}

/// Handles field separation ( -f -d )
fn field_proc(
    ch: &mut PipeIntercepter,
    line: &Vec<u8>,
    delim: &str,
    ranges: &Vec<list::ranges::Range>,
) -> Result<(), errors::TokenSendError> {
    let line = String::from_utf8_lossy(&line).to_string();
    let tokens = line.split(delim);
    let mut ri = 0;
    for (i, token) in tokens.enumerate() {
        if i > 0 {
            ch.send_msg(delim.to_string())?;
        }
        if ranges[ri].high < (i + 1) && (ri + 1) < ranges.len() {
            ri += 1;
        }
        if ranges[ri].low <= (i + 1) && (i + 1) <= ranges[ri].high {
            // Should empty filed sent as empty string ? Discussion is needed.
            // But author(@greymd) believes empty string is good to be sent.
            // Because teip can be used as simple CSV file editor if it is allowed!
            // ```
            // $ printf ',,,\n,,,\n,,,\n' | teip -d, -f1- -- seq 12
            // 1,2,3,4
            // 5,6,7,8
            // 9,10,11,12
            // ```
            ch.send_pipe(token.to_string())?;
        } else {
            ch.send_msg(token.to_string())?;
        }
    }
    Ok(())
}

pub fn trim_eol(buf: &mut Vec<u8>) -> String {
    if buf.ends_with(&[b'\r', b'\n']) {
        buf.pop();
        buf.pop();
        return "\r\n".to_string();
    }
    if buf.ends_with(&[b'\n']) {
        buf.pop();
        return "\n".to_string();
    }
    if buf.ends_with(&[b'\0']) {
        buf.pop();
        return "\0".to_string();
    }
    "".to_string()
}

fn vecstr_rm_references(orig: &Vec<&str>) -> Vec<String> {
    let mut removed: Vec<String> = Vec::new();
    for c in orig {
        removed.push(c.to_string());
    }
    removed
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_trim_eol() {
        let mut buf = vec![b'\x61', b'\x62', b'\n'];
        let end = trim_eol(&mut buf);
        assert_eq!(String::from_utf8_lossy(&buf).to_string(), "ab");
        assert_eq!(end, "\n");
    }
}
