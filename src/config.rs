use std::{
    fs::{canonicalize, read_to_string},
    net::SocketAddr,
    path::PathBuf,
    process,
    str::FromStr,
};

use crypto_mac::{InvalidKeyLength, Mac};
use hmac::Hmac;
use lrlex::lrlex_mod;
use lrpar::{lrpar_mod, Lexer, Span};
use regex::Regex;
use secstr::SecStr;
use sha1::Sha1;

use crate::config_ast;

type StorageT = u8;

const DEFAULT_TIMEOUT: u64 = 60 * 60; // 1 hour

lrlex_mod!("config.l");
lrpar_mod!("config.y");

pub struct Config {
    /// The IP address/port on which to listen.
    pub listen: SocketAddr,
    /// The maximum number of parallel jobs to run.
    pub maxjobs: usize,
    /// The GitHub block.
    pub github: GitHub,
    /// The Unix user to change to after snare has bound itself to a network port.
    pub user: Option<String>,
}

impl Config {
    /// Create a `Config` from `path`, returning `Err(String)` (containing a human readable
    /// message) if it was unable to do so.
    pub fn from_path(conf_path: &PathBuf) -> Result<Self, String> {
        let input = match read_to_string(conf_path) {
            Ok(s) => s,
            Err(e) => return Err(format!("Can't read {:?}: {}", conf_path, e)),
        };

        let lexerdef = config_l::lexerdef();
        let lexer = lexerdef.lexer(&input);
        let (astopt, errs) = config_y::parse(&lexer);
        if !errs.is_empty() {
            let msgs = errs
                .iter()
                .map(|e| e.pp(&lexer, &config_y::token_epp))
                .collect::<Vec<_>>();
            return Err(msgs.join("\n"));
        }
        let mut github = None;
        let mut listen = None;
        let mut maxjobs = None;
        let mut user = None;
        match astopt {
            Some(Ok(opts)) => {
                for opt in opts {
                    match opt {
                        config_ast::TopLevelOption::GitHub(span, options, matches) => {
                            if github.is_some() {
                                return Err(error_at_span(
                                    &lexer,
                                    span,
                                    "Mustn't specify 'github' more than once",
                                ));
                            }
                            github = Some(GitHub::parse(&lexer, options, matches)?);
                        }
                        config_ast::TopLevelOption::Listen(span) => {
                            if listen.is_some() {
                                return Err(error_at_span(
                                    &lexer,
                                    span,
                                    "Mustn't specify 'listen' more than once",
                                ));
                            }
                            let listen_str = lexer.span_str(span);
                            let listen_str = &listen_str[1..listen_str.len() - 1];
                            match SocketAddr::from_str(listen_str) {
                                Ok(l) => listen = Some(l),
                                Err(e) => {
                                    return Err(error_at_span(
                                        &lexer,
                                        span,
                                        &format!("Invalid listen address '{}': {}", listen_str, e),
                                    ));
                                }
                            }
                        }
                        config_ast::TopLevelOption::MaxJobs(span) => {
                            if maxjobs.is_some() {
                                return Err(error_at_span(
                                    &lexer,
                                    span,
                                    "Mustn't specify 'maxjobs' more than once",
                                ));
                            }
                            let maxjobs_str = lexer.span_str(span);
                            match maxjobs_str.parse() {
                                Ok(0) => {
                                    return Err(error_at_span(
                                        &lexer,
                                        span,
                                        "Must allow at least 1 job",
                                    ))
                                }
                                Ok(x) if x > (std::usize::MAX - 1) / 2 => {
                                    return Err(error_at_span(
                                        &lexer,
                                        span,
                                        &format!(
                                            "Maximum number of jobs is {}",
                                            (std::usize::MAX - 1) / 2
                                        ),
                                    ))
                                }
                                Ok(x) => maxjobs = Some(x),
                                Err(e) => {
                                    return Err(error_at_span(&lexer, span, &format!("{}", e)))
                                }
                            }
                        }
                        config_ast::TopLevelOption::User(span) => {
                            if user.is_some() {
                                return Err(error_at_span(
                                    &lexer,
                                    span,
                                    "Mustn't specify 'user' more than once",
                                ));
                            }
                            let user_str = lexer.span_str(span);
                            let user_str = &user_str[1..user_str.len() - 1];
                            user = Some(user_str.to_owned());
                        }
                    }
                }
            }
            _ => process::exit(1),
        }
        let maxjobs = maxjobs.unwrap_or_else(|| num_cpus::get());
        let listen = listen.ok_or_else(|| "A 'listen' address must be specified".to_owned())?;
        let github = github.ok_or_else(|| {
            "A GitHub block with at least a 'repodirs' option must be specified".to_owned()
        })?;

        Ok(Config {
            listen,
            maxjobs,
            github,
            user,
        })
    }
}

pub struct GitHub {
    /// A *fully canonicalised* path to the directory containing per-repo programs.
    pub reposdir: String,
    pub matches: Vec<Match>,
}

impl GitHub {
    fn parse(
        lexer: &dyn Lexer<StorageT>,
        options: Vec<config_ast::ProviderOption>,
        ast_matches: Vec<config_ast::Match>,
    ) -> Result<Self, String> {
        let mut reposdir = None;
        let mut matches = vec![Match::default()];

        for option in options {
            match option {
                config_ast::ProviderOption::ReposDir(span) => {
                    if reposdir.is_some() {
                        return Err(error_at_span(
                            lexer,
                            span,
                            "Mustn't specify 'reposdir' more than once",
                        ));
                    }

                    let reposdir_str = lexer.span_str(span);
                    let reposdir_str = &reposdir_str[1..reposdir_str.len() - 1];
                    reposdir = Some(match canonicalize(reposdir_str) {
                        Ok(p) => match p.to_str() {
                            Some(s) => s.to_owned(),
                            None => {
                                return Err(format!("'{}': can't convert to string", &reposdir_str))
                            }
                        },
                        Err(e) => {
                            return Err(format!("'{}': {}", reposdir_str, e));
                        }
                    });
                }
            }
        }

        for m in ast_matches {
            let re_str = lexer.span_str(m.re);
            let re_str = format!("^{}$", &re_str[1..re_str.len() - 1]);
            let re = match Regex::new(&re_str) {
                Ok(re) => re,
                Err(e) => {
                    return Err(error_at_span(
                        lexer,
                        m.re,
                        &format!("Regular expression error: {}", e),
                    ))
                }
            };
            let mut email = None;
            let mut queuekind = None;
            let mut secret = None;
            let mut timeout = None;
            for opt in m.options {
                match opt {
                    config_ast::PerRepoOption::Email(span) => {
                        if email.is_some() {
                            return Err(error_at_span(
                                lexer,
                                span,
                                "Mustn't specify 'email' more than once",
                            ));
                        }
                        let email_str = lexer.span_str(span);
                        let email_str = &email_str[1..email_str.len() - 1];
                        email = Some(email_str.to_owned());
                    }
                    config_ast::PerRepoOption::Queue(span, qkind) => {
                        if queuekind.is_some() {
                            return Err(error_at_span(
                                lexer,
                                span,
                                "Mustn't specify 'queue' more than once",
                            ));
                        }
                        queuekind = Some(match qkind {
                            config_ast::QueueKind::Evict => QueueKind::Evict,
                            config_ast::QueueKind::Parallel => QueueKind::Parallel,
                            config_ast::QueueKind::Sequential => QueueKind::Sequential,
                        });
                    }
                    config_ast::PerRepoOption::Secret(span) => {
                        if secret.is_some() {
                            return Err(error_at_span(
                                lexer,
                                span,
                                "Mustn't specify 'secret' more than once",
                            ));
                        }
                        let sec_str = lexer.span_str(span);
                        let sec_str = &sec_str[1..sec_str.len() - 1];

                        // Looking at the Hmac code, it seems that a key can't actually be of an
                        // invalid length despite the API suggesting that it can be... We're
                        // conservative and assume that it really is possible to have an invalid
                        // length key.
                        match Hmac::<Sha1>::new_varkey(sec_str.as_bytes()) {
                            Ok(_) => (),
                            Err(InvalidKeyLength) => {
                                return Err(error_at_span(lexer, span, "Invalid secret key length"))
                            }
                        }
                        secret = Some(SecStr::from(sec_str));
                    }
                    config_ast::PerRepoOption::Timeout(span) => {
                        if timeout.is_some() {
                            return Err(error_at_span(
                                lexer,
                                span,
                                "Mustn't specify 'timeout' more than once",
                            ));
                        }
                        let t = match lexer.span_str(span).parse() {
                            Ok(t) => t,
                            Err(e) => {
                                return Err(error_at_span(
                                    lexer,
                                    span,
                                    &format!("Invalid timeout: {}", e),
                                ))
                            }
                        };
                        timeout = Some(t);
                    }
                }
            }
            matches.push(Match {
                re,
                email,
                queuekind,
                secret,
                timeout,
            });
        }

        if let Some(reposdir) = reposdir {
            Ok(GitHub { reposdir, matches })
        } else {
            Err("A directory for per-repo programs must be specified".to_owned())
        }
    }

    /// Return a `RepoConfig` for `owner/repo`. Note that if the user reloads the config later,
    /// then a given repository might have two or more `RepoConfig`s with internal settings, so
    /// they should not be mixed. We return the repository's secret as a separate member as it is
    /// relatively costly to clone, and we also prefer not to duplicate it repeatedly throughout
    /// the heap.
    pub fn repoconfig<'a>(&'a self, owner: &str, repo: &str) -> (RepoConfig, Option<&'a SecStr>) {
        let s = format!("{}/{}", owner, repo);
        let mut email = None;
        let mut queuekind = None;
        let mut secret = None;
        let mut timeout = None;
        for m in &self.matches {
            if m.re.is_match(&s) {
                if let Some(ref e) = m.email {
                    email = Some(e.clone());
                }
                if let Some(q) = m.queuekind {
                    queuekind = Some(q);
                }
                if let Some(ref s) = m.secret {
                    secret = Some(s);
                }
                if let Some(t) = m.timeout {
                    timeout = Some(t)
                }
            }
        }
        // Since we know that Matches::default() provides a default queuekind and timeout, both
        // unwraps() are safe.
        (
            RepoConfig {
                email,
                queuekind: queuekind.unwrap(),
                timeout: timeout.unwrap(),
            },
            secret,
        )
    }
}

pub struct Match {
    /// The regular expression to match against full owner/repo names.
    re: Regex,
    /// An optional email address to send errors to.
    email: Option<String>,
    /// The queue kind.
    queuekind: Option<QueueKind>,
    /// The GitHub secret used to validate requests.
    secret: Option<SecStr>,
    /// The maximum time to allow a command to run for before it is terminated (in seconds).
    timeout: Option<u64>,
}

impl Default for Match {
    fn default() -> Self {
        // We know that this Regex is valid so the unwrap() is safe.
        let re = Regex::new(".*").unwrap();
        Match {
            re,
            email: None,
            queuekind: Some(QueueKind::Sequential),
            secret: None,
            timeout: Some(DEFAULT_TIMEOUT),
        }
    }
}

/// Return an error message pinpointing `span` as the culprit.
fn error_at_span(lexer: &dyn Lexer<StorageT>, span: Span, msg: &str) -> String {
    let ((line_off, col), _) = lexer.line_col(span);
    let code = lexer
        .span_lines_str(span)
        .split("\n")
        .nth(0)
        .unwrap()
        .trim();
    format!(
        "Line {}, column {}:\n  {}\n{}",
        line_off,
        col,
        code.trim(),
        msg
    )
}

/// The configuration for a given repository.
pub struct RepoConfig {
    pub email: Option<String>,
    pub queuekind: QueueKind,
    pub timeout: u64,
}

#[derive(Clone, Copy)]
pub enum QueueKind {
    Evict,
    Parallel,
    Sequential,
}
