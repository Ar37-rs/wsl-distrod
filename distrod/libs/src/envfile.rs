use nom::{
    branch::alt,
    bytes::complete::{is_not, tag, take, take_while, take_while1},
    character::{
        complete::{char, line_ending, none_of, space0, space1},
        is_alphabetic, is_digit, is_newline,
    },
    combinator::{map_res, opt, recognize},
    multi::{many1, separated_list0},
    sequence::{pair, separated_pair, terminated, tuple},
    IResult,
};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    ops::{Deref, DerefMut},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone, Default)]
pub struct EnvShellScript {
    envs: HashMap<String, String>,
    paths: HashMap<String, bool>,
}

impl EnvShellScript {
    pub fn new() -> Self {
        EnvShellScript::default()
    }

    pub fn put_env(&mut self, key: String, value: String) {
        self.envs.insert(key, value);
    }

    pub fn put_path(&mut self, path: String, prepends: bool) {
        self.paths.insert(path, prepends);
    }

    pub fn write<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let mut file = BufWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .mode(0o755)
                .open(path.as_ref())
                .with_context(|| format!("Failed to create {:?}.", path.as_ref()))?,
        );
        let script = self.gen_shell_script();
        file.write_all(script.as_bytes())?;

        Ok(())
    }

    fn gen_shell_script(&self) -> String {
        let mut script = String::new();
        let mut envs: Vec<(_, _)> = self.envs.iter().collect();
        envs.sort_by(|(key_a, _), (key_b, _)| key_a.cmp(key_b));
        for (key, value) in envs {
            script.push_str(&format!(
                "if [ -z \"${{{}:-}}\" ]; then export {}={}; fi\n",
                key,
                key,
                single_quote_str_for_shell(value)
            ));
        }
        let mut paths: Vec<_> = self.paths.iter().collect();
        paths.sort();
        for (path, prepends) in paths {
            script.push_str(&format!(
                "__CANDIDATE_PATH={}\n\
                 __COLON_PATH=\":${{PATH}}:\"\n",
                single_quote_str_for_shell(path)
            ));
            if *prepends {
                script.push_str(
                 "if [ \"${__COLON_PATH#*:${__CANDIDATE_PATH}:}\" = \"${__COLON_PATH}\" ]; then export PATH=\"${__CANDIDATE_PATH}:${PATH}\"; fi\n"
                );
            } else {
                script.push_str(
                 "if [ \"${__COLON_PATH#*:${__CANDIDATE_PATH}:}\" = \"${__COLON_PATH}\" ]; then export PATH=\"${PATH}:${__CANDIDATE_PATH}\"; fi\n"
                );
            }
            script.push_str(
                "unset __CANDIDATE_PATH\n\
                 unset __COLON_PATH\n",
            );
        }
        script
    }
}

/// EnvFile understands /etc/environment at about the same level as pam_env.so,
/// so that it can modify the value of existing environment variables or add new ones.
/// (See https://github.com/linux-pam/linux-pam/blob/master/modules/pam_env/pam_env.c)
#[derive(Debug, Clone)]
pub struct EnvFile {
    pub file_path: PathBuf,
    envs: HashMap<String, usize>,
    env_file_lines: EnvFileLines,
}

#[derive(Debug, Clone, Default)]
struct EnvFileLines(Vec<EnvFileLine>);

#[derive(Debug, Clone)]
enum EnvFileLine {
    Env(EnvStatement),
    Other(String),
}

#[derive(Debug, Clone)]
struct EnvStatement {
    key: String,
    value: String,
    leading_characters: String,
    following_characters: String,
}

impl EnvFile {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<EnvFile> {
        let file = File::open(path.as_ref());
        if matches!(file, Err(ref e) if e.kind() == std::io::ErrorKind::NotFound) {
            return Ok(EnvFile {
                file_path: path.as_ref().to_owned(),
                envs: HashMap::<String, usize>::default(),
                env_file_lines: EnvFileLines::default(),
            });
        }

        let file = file.with_context(|| format!("Failed to open {:?}", path.as_ref()))?;
        let mut reader = BufReader::new(file);
        let mut buf = vec![];
        reader
            .read_to_end(&mut buf)
            .with_context(|| format!("Failed to read {:?}", path.as_ref()))?;

        let env_file_lines = EnvFileLines::parse(&buf)
            .map_err(|e| anyhow!("Failed to parse a line: {:?}", e))?
            .1;
        let mut envs = HashMap::<String, usize>::default();
        for (i, line) in env_file_lines.iter().enumerate() {
            if let EnvFileLine::Env(env) = line {
                envs.insert(env.key.clone(), i);
            };
        }

        Ok(EnvFile {
            file_path: path.as_ref().to_owned(),
            envs,
            env_file_lines,
        })
    }

    pub fn get_env(&self, key: &str) -> Option<&str> {
        let val = match self.env_file_lines[*self.envs.get(key)?] {
            EnvFileLine::Env(ref env_statement) => env_statement.value.as_str(),
            _ => unreachable!(),
        };
        Some(val)
    }

    pub fn put_env(&mut self, key: String, value: String) {
        // we don't allow to put values for safety, otherwise it will confuse pam_env.so and
        // may let other variables be overwritten.
        assert!(!value.contains('\n') && !value.contains('\\'));
        self.put_env_with_no_sanity_check(key, single_quote_str_for_shell(&value))
    }

    pub fn put_path(&mut self, path_val: String) {
        assert!(!path_val
            .chars()
            .any(|chr| ['"', '\'', '\\', '\n'].contains(&chr)));
        const DEFAULT_PATH: &str = "'/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games'";
        let pathenv_value = {
            let mut path_variable =
                PathVariable::parse(self.get_env("PATH").unwrap_or(DEFAULT_PATH));
            path_variable.put_path(&path_val);
            path_variable.serialize()
        };
        self.put_env_with_no_sanity_check("PATH".to_owned(), pathenv_value);
    }

    fn put_env_with_no_sanity_check(&mut self, key: String, value: String) {
        let line_index = self.envs.get(&key);
        match line_index {
            Some(index) => {
                let line = &mut self.env_file_lines[*index];
                match *line {
                    EnvFileLine::Env(ref mut env_statement) => {
                        env_statement.value = value;
                    }
                    _ => unreachable!(),
                }
            }
            None => {
                let line = EnvFileLine::Env(EnvStatement {
                    key: key.clone(),
                    value,
                    leading_characters: String::new(),
                    following_characters: String::new(),
                });
                self.env_file_lines.push(line);
                self.envs.insert(key, self.env_file_lines.len() - 1);
            }
        }
    }

    pub fn write(&mut self) -> Result<()> {
        let mut file = BufWriter::new(
            File::create(&self.file_path)
                .with_context(|| format!("Failed to create {:?}.", &self.file_path))?,
        );
        file.write_all(self.env_file_lines.serialize().as_bytes())?;
        Ok(())
    }
}

impl EnvFileLines {
    pub fn parse(input: &[u8]) -> IResult<&[u8], EnvFileLines> {
        if input.is_empty() {
            return Ok((&[], EnvFileLines(vec![])));
        }
        map_res::<_, _, _, _, nom::Err<&[u8]>, _, _>(many1(EnvFileLine::parse), |lines| {
            Ok(EnvFileLines(lines))
        })(input)
    }

    pub fn serialize(&self) -> String {
        let lines = self.0.iter().map(|l| l.serialize()).collect::<Vec<_>>();
        lines.join("")
    }
}

impl Deref for EnvFileLines {
    type Target = Vec<EnvFileLine>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for EnvFileLines {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl EnvFileLine {
    pub fn parse(line: &[u8]) -> IResult<&[u8], EnvFileLine> {
        let other_line = map_res::<_, _, _, _, nom::Err<&[u8]>, _, _>(
            alt((
                // line with a comment or other strings with or without a line ending
                terminated(recognize(many1(is_not("\n"))), opt(line_ending)),
                // empty line
                map_res::<_, _, _, _, nom::Err<&[u8]>, _, _>(line_ending, |_| {
                    Ok(<&[u8]>::default())
                }),
            )),
            |s| {
                Ok(EnvFileLine::Other(
                    String::from_utf8_lossy(s).to_string() + "\n",
                ))
            },
        );
        let env = map_res::<_, _, _, _, nom::Err<&[u8]>, _, _>(EnvStatement::parse, |s| {
            Ok(EnvFileLine::Env(s))
        });
        alt((env, other_line))(line)
    }

    pub fn serialize(&self) -> String {
        match *self {
            EnvFileLine::Env(ref env) => env.serialize(),
            EnvFileLine::Other(ref other) => other.clone(),
        }
    }
}

impl EnvStatement {
    pub fn parse(line: &[u8]) -> IResult<&[u8], EnvStatement> {
        let (rest, (leading_characters, (key, value), following_characters, _)) = tuple((
            leading_characters,
            separated_pair(declaration_key, tag("="), declaration_value),
            following_characters,
            opt(line_ending),
        ))(line)?;
        let to_string = |s: &[u8]| -> String { String::from_utf8_lossy(s).to_string() };
        Ok((
            rest,
            EnvStatement {
                key: to_string(key),
                value: to_string(value),
                leading_characters: to_string(leading_characters),
                following_characters: to_string(following_characters),
            },
        ))
    }

    pub fn serialize(&self) -> String {
        let mut serialized_line = self.leading_characters.clone();
        serialized_line.push_str(&self.key);
        serialized_line.push('=');
        serialized_line.push_str(&self.value);
        serialized_line.push_str(&self.following_characters);
        serialized_line.push('\n');
        serialized_line
    }
}

fn leading_characters(line: &[u8]) -> IResult<&[u8], &[u8]> {
    recognize(tuple((space0, opt(tag(b"export")), space0)))(line)
}

fn declaration_key(line: &[u8]) -> IResult<&[u8], &[u8]> {
    take_while1(|c| is_alphabetic(c) || is_digit(c) || c == b'_')(line)
}

fn declaration_value(line: &[u8]) -> IResult<&[u8], &[u8]> {
    //let regular_char = take_while(|c| !is_space(c) && !is_newline(c) && c != b'#');
    let escaped_char = recognize(pair(char('\\'), take(1u32)));
    let regular_char = recognize(none_of("\n# \t\\"));
    recognize(separated_list0(
        space1,
        many1(alt((regular_char, escaped_char))),
    ))(line)
}

fn following_characters(line: &[u8]) -> IResult<&[u8], &[u8]> {
    take_while(|c| !is_newline(c))(line)
}

#[derive(Debug, Clone)]
pub struct PathVariable<'a> {
    parsed_paths: Vec<&'a str>,
    added_paths: Vec<&'a str>,
    path_set: HashSet<&'a str>,
    surrounding_quote: Option<char>,
}

impl<'a> PathVariable<'a> {
    pub fn parse(val: &'a str) -> Self {
        let mut paths: Vec<_> = val.split(':').into_iter().collect();

        // Roughly regard the whole path is surrounded by double quotes by simple logic
        let quote_candidates = vec!['"', '\''];
        let surrounding_quote = quote_candidates.into_iter().find(|quote| {
            paths.first().map_or(false, |path| {
                path.starts_with(*quote) && !path.ends_with(*quote)
            }) && paths.last().map_or(false, |path| {
                !path.starts_with(*quote) && path.ends_with(*quote)
            })
        });

        if surrounding_quote.is_some() {
            paths[0] = &paths[0][1..];
            let len = paths.len();
            paths[len - 1] = &paths[len - 1][..paths[len - 1].len() - 1];
        }

        let mut path_set = HashSet::<&str>::new();
        for path in paths.iter() {
            path_set.insert(*path);
        }

        PathVariable {
            parsed_paths: paths,
            added_paths: vec![],
            path_set,
            surrounding_quote,
        }
    }

    pub fn serialize(&self) -> String {
        let mut path_var = self
            .added_paths
            .iter()
            .map(|path| self.quote_path_if_necessary(path))
            .rev()
            .chain(self.parsed_paths.iter().map(|path| path.to_string()))
            .collect::<Vec<_>>()
            .join(":");

        if let Some(quote) = self.surrounding_quote {
            path_var.insert(0, quote);
            path_var.push(quote);
        }

        path_var
    }

    fn quote_path_if_necessary(&self, path: &str) -> String {
        if self.surrounding_quote.is_none() {
            return single_quote_str_for_shell(path);
        }
        path.to_owned()
    }

    pub fn put_path(&mut self, path_val: &'a str) {
        if self.path_set.contains(path_val) {
            return;
        }
        self.added_paths.push(path_val);
        self.path_set
            .insert(self.added_paths[self.added_paths.len() - 1]);
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.added_paths
            .iter()
            .rev()
            .chain(self.parsed_paths.iter())
            .copied()
    }
}

fn single_quote_str_for_shell(s: &str) -> String {
    format!("'{}'", s.replace("'", "'\"'\"'"))
}

#[cfg(test)]
mod test_env_shell_script {
    use super::*;

    #[test]
    fn test_simple_env_shell_script() {
        let mut env_shell_script = EnvShellScript::new();
        env_shell_script.put_env("var1".to_owned(), "val1".to_owned());
        env_shell_script.put_env("var2".to_owned(), "val2".to_owned());
        env_shell_script.put_env("var_space".to_owned(), "value with space".to_owned());
        env_shell_script.put_env("var2".to_owned(), "val2 again".to_owned());

        env_shell_script.put_path("/path/to/somewhere".to_owned(), true);
        env_shell_script.put_path("/path/with space/somewhere".to_owned(), true);
        env_shell_script.put_path("/path/to/somewhere".to_owned(), false);
        env_shell_script.put_path("/less_prio/path".to_owned(), false);

        let script = env_shell_script.gen_shell_script();
        assert_eq!(
            "if [ -z \"${var1:-}\" ]; then export var1='val1'; fi\n\
             if [ -z \"${var2:-}\" ]; then export var2='val2 again'; fi\n\
             if [ -z \"${var_space:-}\" ]; then export var_space='value with space'; fi\n\
             __CANDIDATE_PATH='/less_prio/path'\n\
             __COLON_PATH=\":${PATH}:\"\n\
             if [ \"${__COLON_PATH#*:${__CANDIDATE_PATH}:}\" = \"${__COLON_PATH}\" ]; then export PATH=\"${PATH}:${__CANDIDATE_PATH}\"; fi\n\
             unset __CANDIDATE_PATH\n\
             unset __COLON_PATH\n\
             __CANDIDATE_PATH='/path/to/somewhere'\n\
             __COLON_PATH=\":${PATH}:\"\n\
             if [ \"${__COLON_PATH#*:${__CANDIDATE_PATH}:}\" = \"${__COLON_PATH}\" ]; then export PATH=\"${PATH}:${__CANDIDATE_PATH}\"; fi\n\
             unset __CANDIDATE_PATH\n\
             unset __COLON_PATH\n\
             __CANDIDATE_PATH='/path/with space/somewhere'\n\
             __COLON_PATH=\":${PATH}:\"\n\
             if [ \"${__COLON_PATH#*:${__CANDIDATE_PATH}:}\" = \"${__COLON_PATH}\" ]; then export PATH=\"${__CANDIDATE_PATH}:${PATH}\"; fi\n\
             unset __CANDIDATE_PATH\n\
             unset __COLON_PATH\n",
            &script
        );
    }

    #[test]
    fn test_script_by_shell() {
        let mut env_shell_script = EnvShellScript::new();
        env_shell_script.put_env("var_space".to_owned(), "value with space".to_owned());
        env_shell_script.put_env("existing_var".to_owned(), "updated".to_owned());
        env_shell_script.put_path("/path/to/somewhere".to_owned(), true);
        env_shell_script.put_path("/path/with space/somewhere".to_owned(), true);
        env_shell_script.put_path("/path/with space/somewhere".to_owned(), true);
        env_shell_script.put_path("/bin".to_owned(), true);

        let mut script = env_shell_script.gen_shell_script();
        script.push_str(
            "\
            echo $var_space\n\
            echo $existing_var\n\
            echo $PATH\n\
        ",
        );

        let mut shell = std::process::Command::new("sh");
        shell.arg("-c");
        shell.arg(&script);
        shell.env("existing_var", "not updated");
        shell.env("PATH", "/usr/local/bin:/sbin:/bin");
        let output = shell.output().unwrap();
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(
            "value with space\nnot updated\n/path/with space/somewhere:/path/to/somewhere:/usr/local/bin:/sbin:/bin\n",
            &String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[cfg(test)]
mod test_path_variable {
    use super::*;

    #[test]
    fn test_simple_variable() {
        let path_value = "/usr/local/bin:/usr/bin:/sbin:/bin";
        let mut path = PathVariable::parse(path_value);
        assert_eq!(path_value, path.serialize().as_str());

        path.put_path("/new/path1/bin");
        path.put_path("/new/path2/bin");
        path.put_path("/new/path2/bin"); // Put the same path again
        assert_eq!(
            format!("'/new/path2/bin':'/new/path1/bin':{}", path_value),
            path.serialize()
        );

        assert_eq!(
            vec![
                "/new/path2/bin",
                "/new/path1/bin",
                "/usr/local/bin",
                "/usr/bin",
                "/sbin",
                "/bin"
            ],
            path.iter().collect::<Vec<&str>>()
        );
    }

    #[test]
    fn test_add_existing_value() {
        let path_value = "/usr/local/bin:/usr/bin:/sbin:/bin";
        let mut path = PathVariable::parse(path_value);
        assert_eq!(path_value, path.serialize().as_str());
        path.put_path("/usr/local/bin");
        assert_eq!("/usr/local/bin:/usr/bin:/sbin:/bin", path.serialize());

        let path_value = "'/usr/local/bin:/usr/bin:/sbin:/bin'";
        let mut path = PathVariable::parse(path_value);
        assert_eq!(path_value, path.serialize().as_str());
        path.put_path("/usr/local/bin");
        assert_eq!("'/usr/local/bin:/usr/bin:/sbin:/bin'", path.serialize());
    }

    #[test]
    fn test_quoted_variable() {
        // quoted simple value
        let path_value = "\"/usr/local/bin:/usr/bin:/sbin:/bin\"";
        let mut path = PathVariable::parse(path_value);
        assert_eq!(path_value, path.serialize());
        assert_eq!(
            vec!["/usr/local/bin", "/usr/bin", "/sbin", "/bin"],
            path.iter().collect::<Vec<&str>>()
        );

        path.put_path("/new/path1/bin");
        path.put_path("/new/path2/bin");
        assert_eq!(
            format!(
                "\"/new/path2/bin:/new/path1/bin:{}\"",
                &path_value[1..path_value.len() - 1]
            ),
            path.serialize()
        );

        // single quote
        let path_value = "'/usr/local/bin:/usr/bin:/sbin:/bin'";
        let mut path = PathVariable::parse(path_value);
        path.put_path("/new/path1/bin");
        assert_eq!(
            "'/new/path1/bin:/usr/local/bin:/usr/bin:/sbin:/bin'",
            path.serialize()
        );
        assert_eq!(
            vec![
                "/new/path1/bin",
                "/usr/local/bin",
                "/usr/bin",
                "/sbin",
                "/bin"
            ],
            path.iter().collect::<Vec<&str>>()
        );
    }

    #[test]
    fn test_value_not_quoted_as_a_whole() {
        let path_value = "\"/mnt/c/Program Files/foo\":/usr/local/bin:/usr/bin:/sbin:/bin";
        let path = PathVariable::parse(path_value);
        assert_eq!(path_value, path.serialize());

        assert_eq!(
            vec![
                "\"/mnt/c/Program Files/foo\"",
                "/usr/local/bin",
                "/usr/bin",
                "/sbin",
                "/bin",
            ],
            path.iter().collect::<Vec<&str>>()
        );

        let path_value = "/usr/local/bin:/usr/bin:/sbin:/bin:\"/mnt/c/Program Files/foo\"";
        let path = PathVariable::parse(path_value);
        assert_eq!(path_value, path.serialize());

        assert_eq!(
            vec![
                "/usr/local/bin",
                "/usr/bin",
                "/sbin",
                "/bin",
                "\"/mnt/c/Program Files/foo\"",
            ],
            path.iter().collect::<Vec<&str>>()
        );

        let path_value = "\"/usr/local/bin\":/usr/bin:/sbin:/bin:\"/mnt/c/Program Files/foo\"";
        let path = PathVariable::parse(path_value);
        assert_eq!(path_value, path.serialize());

        assert_eq!(
            vec![
                "\"/usr/local/bin\"",
                "/usr/bin",
                "/sbin",
                "/bin",
                "\"/mnt/c/Program Files/foo\"",
            ],
            path.iter().collect::<Vec<&str>>()
        );

        // quoted single value is treated as "a value the first value of which is quoted", so it's not
        // quoted "as a whole"
        let path_value = "\"/bin\"";
        let mut path = PathVariable::parse(path_value);
        assert_eq!(path_value, path.serialize());

        assert_eq!(vec!["\"/bin\""], path.iter().collect::<Vec<&str>>());

        path.put_path("/new/path1/space bin");
        path.put_path("/new/path2/bin");
        assert_eq!(
            "'/new/path2/bin':'/new/path1/space bin':\"/bin\"",
            path.serialize()
        );

        // Don't support too tricky values
        let path_value =
            "\"/mnt/c/Program Files\"/foo:/usr/bin:/sbin:/bin:/some/path/include/quote\\\"";
        let mut path = PathVariable::parse(path_value);
        path.put_path("/usr/local/bin");
        assert_ne!("'/usr/local/bin':\"/mnt/c/Program Files\"/foo:/usr/bin:/sbin:/bin:/some/path/include/quote\\\"", path.serialize());
    }
}

#[cfg(test)]
mod test_env_file_parsers {
    use super::*;

    #[test]
    fn test_parse_env_statement_simple() {
        let (_, statement) = EnvStatement::parse("PATH=hoge:fuga:piyo".as_bytes()).unwrap();
        eprintln!("Statement: {:#?}", &statement);
        assert_eq!("PATH", statement.key);
        assert_eq!("hoge:fuga:piyo", statement.value);
        assert_eq!("", statement.leading_characters);
        assert_eq!("", statement.following_characters);
        assert_eq!("PATH=hoge:fuga:piyo\n", statement.serialize());

        // same value with new line
        let (_, statement) = EnvStatement::parse("PATH=hoge:fuga:piyo\n".as_bytes()).unwrap();
        eprintln!("Statement: {:#?}", &statement);
        assert_eq!("PATH", statement.key);
        assert_eq!("hoge:fuga:piyo", statement.value);
        assert_eq!("", statement.leading_characters);
        assert_eq!("", statement.following_characters);
        assert_eq!("PATH=hoge:fuga:piyo\n", statement.serialize());

        // with comment and exprot
        let (_, statement) =
            EnvStatement::parse(" export  PATH=hoge:fuga:piyo  # comment".as_bytes()).unwrap();
        eprintln!("Statement: {:#?}", &statement);
        assert_eq!("PATH", statement.key);
        assert_eq!("hoge:fuga:piyo", statement.value);
        assert_eq!(" export  ", statement.leading_characters);
        assert_eq!("  # comment", statement.following_characters);
        assert_eq!(
            " export  PATH=hoge:fuga:piyo  # comment\n",
            statement.serialize()
        );
    }

    #[test]
    fn test_parse_env_statement_empty() {
        assert!(EnvStatement::parse("".as_bytes()).is_err());

        let (_, statement) = EnvStatement::parse("PATH=".as_bytes()).unwrap();
        eprintln!("Statement: {:#?}", &statement);
        assert_eq!("PATH", statement.key);
        assert_eq!("", statement.value);
        assert_eq!("", statement.leading_characters);
        assert_eq!("", statement.following_characters);
        assert_eq!("PATH=\n", statement.serialize());

        let (_, statement) = EnvStatement::parse("export PATH=  # no value".as_bytes()).unwrap();
        eprintln!("Statement: {:#?}", &statement);
        assert_eq!("PATH", statement.key);
        assert_eq!("", statement.value);
        assert_eq!("export ", statement.leading_characters);
        assert_eq!("  # no value", statement.following_characters);
        assert_eq!("export PATH=  # no value\n", statement.serialize());
    }

    #[test]
    fn test_parse_env_statement_continued_line() {
        let val = "hoge:fuga:piyo\\\n\
                         :new_line";
        let line = format!("PATH={}  # and comment\n", val);
        let (_, statement) = EnvStatement::parse(line.as_bytes()).unwrap();
        eprintln!("Statement: {:#?}", &statement);
        assert_eq!("PATH", statement.key);
        assert_eq!(val, statement.value);
        assert_eq!("", statement.leading_characters);
        assert_eq!("  # and comment", statement.following_characters);
        assert_eq!(line, statement.serialize());
    }

    #[test]
    fn test_parse_env_statement_strange() {
        let (_, statement) = EnvStatement::parse("VAR=A=B=C".as_bytes()).unwrap();
        eprintln!("Statement: {:#?}", &statement);
        assert_eq!("VAR", statement.key);
        assert_eq!("A=B=C", statement.value);
        assert_eq!("", statement.leading_characters);
        assert_eq!("", statement.following_characters);
        assert_eq!("VAR=A=B=C\n", statement.serialize());

        let (_, statement) = EnvStatement::parse("VAR=A B C # comment".as_bytes()).unwrap();
        eprintln!("Statement: {:#?}", &statement);
        assert_eq!("VAR", statement.key);
        assert_eq!("A B C", statement.value);
        assert_eq!("", statement.leading_characters);
        assert_eq!(" # comment", statement.following_characters);
        assert_eq!("VAR=A B C # comment\n", statement.serialize());

        let (_, statement) = EnvStatement::parse("export VAR=😀 # emoji 😀".as_bytes()).unwrap();
        eprintln!("Statement: {:#?}", &statement);
        assert_eq!("VAR", statement.key);
        assert_eq!("😀", statement.value);
        assert_eq!("export ", statement.leading_characters);
        assert_eq!(" # emoji 😀", statement.following_characters);
        assert_eq!("export VAR=😀 # emoji 😀\n", statement.serialize());
    }

    #[test]
    fn test_parse_env_file_line() {
        let (_, line) = EnvFileLine::parse("# this is comment".as_bytes()).unwrap();
        eprintln!("line: {:#?}", &line);
        assert!(matches!(line, EnvFileLine::Other(_)));
        if let EnvFileLine::Other(str) = &line {
            assert_eq!("# this is comment\n", str);
        }
        assert_eq!("# this is comment\n", line.serialize());

        // empty line
        let (_, line) = EnvFileLine::parse("\n".as_bytes()).unwrap();
        eprintln!("line: {:#?}", &line);
        assert!(matches!(line, EnvFileLine::Other(_)));
        assert_eq!("\n", line.serialize());

        // abnormal line
        let (_, line) = EnvFileLine::parse("==fawe=f= =".as_bytes()).unwrap();
        eprintln!("line: {:#?}", &line);
        assert!(matches!(line, EnvFileLine::Other(_)));
        assert_eq!("==fawe=f= =\n", line.serialize());
    }

    #[test]
    fn test_parse_env_file_lines() {
        let src = "\
        # This is comment\n\
        VAR=VALUE\n\
        \n\
        \n\
        # another comment \n\
        PATH=path1:path2\\\n\
        path3";
        let (_, lines) = EnvFileLines::parse(src.as_bytes()).unwrap();
        eprintln!("lines: {:#?}", &lines);
        assert_eq!(lines.len(), 6);
        assert!(matches!(lines[0], EnvFileLine::Other(_)));
        assert!(matches!(lines[1], EnvFileLine::Env(_)));
        assert!(matches!(lines[2], EnvFileLine::Other(_)));
        assert!(matches!(lines[3], EnvFileLine::Other(_)));
        assert!(matches!(lines[4], EnvFileLine::Other(_)));
        assert!(matches!(lines[5], EnvFileLine::Env(_)));
        assert_eq!(format!("{}\n", src), lines.serialize())
    }
}

#[cfg(test)]
mod test_env_file {
    use super::*;
    use tempfile::*;

    #[test]
    fn test_get() {
        let mut tmp = NamedTempFile::new().unwrap();
        let cont = "\
		    PATH=test:foo:bar\n\
			FOO=foo\n\
			BAR=bar\n\
			BAZ=baz=baz\n\
			FOO=foo2\n\
		";
        write!(&mut tmp, "{}", cont).unwrap();
        let env = EnvFile::open(tmp.path()).unwrap();

        eprintln!("EnvFile: {:#?}", &env);
        assert_eq!(env.get_env("None"), None);
        assert_eq!(env.get_env("PATH"), Some("test:foo:bar"));
        assert_eq!(env.get_env("BAZ"), Some("baz=baz"));
        assert_eq!(
            env.get_env("FOO"),
            Some("foo2"),
            "The last value is obtained if the environment has multiple values."
        );
    }

    #[test]
    fn test_put_env_and_save() {
        let mut tmp = NamedTempFile::new().unwrap();
        let cont = "\
            # This is a comment line
		    PATH=test:foo:bar  #comment preserved \n\
            WSL_INTEROP=/run/foo\n\
			FOO=foo\n\
            # This is another comment line
			BAR=bar\n\
			BAZ=baz=baz\n\
            QUOTED1='foo'\n\
            QUOTED2=\"foo\"\n\
			FOO=foo1\n\
		";
        write!(&mut tmp, "{}", cont).unwrap();
        let mut env = EnvFile::open(tmp.path()).unwrap();

        env.put_env("NEW1".to_owned(), "TO_BE_OVERWRITTEN".to_owned());
        env.put_env(
            "PATH".to_owned(),
            format!("path:{}", env.get_env("PATH").unwrap()),
        );
        env.put_env("FOO".to_owned(), "foo2".to_owned());
        env.put_env("FOO".to_owned(), "foo3".to_owned());
        env.put_env("BAR".to_owned(), "bar2".to_owned());
        env.put_env("NEW1".to_owned(), "NEW1".to_owned());
        env.put_env("QUOTED1".to_owned(), "quoted1".to_owned());
        env.put_env("QUOTED2".to_owned(), "quoted2".to_owned());
        env.put_env("WSL_INTEROP".to_owned(), "/run/bar".to_owned());

        assert_eq!(env.get_env("None"), None);
        assert_eq!(env.get_env("NEW1"), Some("'NEW1'"));
        assert_eq!(env.get_env("PATH"), Some("'path:test:foo:bar'"));
        assert_eq!(env.get_env("FOO"), Some("'foo3'"));

        env.write().unwrap();
        let expected = "\
            # This is a comment line
		    PATH='path:test:foo:bar'  #comment preserved \n\
            WSL_INTEROP='/run/bar'\n\
			FOO=foo\n\
            # This is another comment line
			BAR='bar2'\n\
			BAZ=baz=baz\n\
            QUOTED1='quoted1'\n\
            QUOTED2='quoted2'\n\
			FOO='foo3'\n\
			NEW1='NEW1'\n\
		";
        let new_cont = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(expected, new_cont);
    }

    #[test]
    fn test_put_path() {
        let mut tmp = NamedTempFile::new().unwrap();
        let cont = "\
            # This is a comment line\n\
            PATH=\"/sbin:/bin\"\n\
			FOO=foo\n\
			BAR=bar\n\
		";
        write!(&mut tmp, "{}", cont).unwrap();
        let mut env = EnvFile::open(tmp.path()).unwrap();

        env.put_path("/to/path1".to_owned());
        env.put_path("/to/path2".to_owned());
        env.put_path("/sbin".to_owned());

        assert_eq!(
            Some("\"/to/path2:/to/path1:/sbin:/bin\""),
            env.get_env("PATH")
        );

        env.write().unwrap();
        let expected = "\
            # This is a comment line\n\
            PATH=\"/to/path2:/to/path1:/sbin:/bin\"\n\
			FOO=foo\n\
			BAR=bar\n\
		";
        let new_cont = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(new_cont, expected);
    }

    #[test]
    fn test_put_path_no_quote() {
        let mut tmp = NamedTempFile::new().unwrap();
        let cont = "\
            # This is a comment line\n\
            PATH=/sbin:/bin\n\
			FOO=foo\n\
			BAR=bar\n\
		";
        write!(&mut tmp, "{}", cont).unwrap();
        let mut env = EnvFile::open(tmp.path()).unwrap();

        env.put_path("/to/path with space".to_owned());

        env.write().unwrap();
        let expected = "\
            # This is a comment line\n\
            PATH='/to/path with space':/sbin:/bin\n\
			FOO=foo\n\
			BAR=bar\n\
		";
        let new_cont = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(new_cont, expected);
    }

    #[test]
    fn test_put_path_strange() {
        let mut tmp = NamedTempFile::new().unwrap();
        let cont = "\
            # This is a comment line\n\
            PATH=/sbin:/bin:\\\n\
            /other/bin  #continued PATH\n\
			FOO=foo\n\
			BAR=bar\n\
		";
        write!(&mut tmp, "{}", cont).unwrap();
        let mut env = EnvFile::open(tmp.path()).unwrap();

        env.put_path("/to/path with space".to_owned());

        env.write().unwrap();
        let expected = "\
            # This is a comment line\n\
            PATH='/to/path with space':/sbin:/bin:\\\n\
            /other/bin  #continued PATH\n\
			FOO=foo\n\
			BAR=bar\n\
		";
        let new_cont = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(new_cont, expected);
    }

    #[test]
    fn test_put_path_to_no_path_file() {
        let mut tmp = NamedTempFile::new().unwrap();
        let cont = "\
            # This is a comment line
			FOO=foo\n\
			BAR=bar\n\
		";
        write!(&mut tmp, "{}", cont).unwrap();
        let mut env = EnvFile::open(tmp.path()).unwrap();

        env.put_path("/to/path1".to_owned());
        env.put_path("/to/path2".to_owned());

        assert_eq!(Some("'/to/path2:/to/path1:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games'"), env.get_env("PATH"));

        env.write().unwrap();
        let expected = "\
            # This is a comment line
			FOO=foo\n\
			BAR=bar\n\
            PATH='/to/path2:/to/path1:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games'\n\
		";
        let new_cont = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(new_cont, expected);
    }

    #[test]
    fn test_empty_env_file() {
        let tmp = NamedTempFile::new().unwrap();
        let env = EnvFile::open(tmp.path());
        assert!(env.is_ok());

        let mut env = env.unwrap();
        env.put_env("TEST".to_owned(), "VALUE".to_owned());
        env.write().unwrap();
        let expected = "\
		    TEST='VALUE'\n\
		";
        let new_cont = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(new_cont, expected);
    }

    #[test]
    fn test_open_nonexistential_env_file() {
        let tmpdir = TempDir::new().unwrap();
        let env = EnvFile::open(tmpdir.path().join("dont_exist"));
        assert!(env.is_ok());

        let mut env = env.unwrap();
        env.put_env("TEST".to_owned(), "VALUE".to_owned());
        env.write().unwrap();
        let expected = "\
		    TEST='VALUE'\n\
		";
        let new_cont = std::fs::read_to_string(tmpdir.path().join("dont_exist")).unwrap();
        assert_eq!(new_cont, expected);
    }
}
