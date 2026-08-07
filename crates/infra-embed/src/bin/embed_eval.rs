//! Stage 2 candidate-model eval driver (RFC 0007 §9/§10).
//!
//! Subcommands:
//!   fetch  <repo> <revision> --out DIR   download model.safetensors,
//!                                        config.json, tokenizer.json and
//!                                        print SHA-256 digests (the eventual
//!                                        manifest is authored from these)
//!   embed  --dir DIR [pooling/prefix/dims/max-tokens flags] --side query|corpus
//!                                        read a JSON array of strings on
//!                                        stdin, write a JSON array of
//!                                        normalized vectors on stdout
//!
//! Eval-mode fetch does not pin digests (no manifest exists until a winner is
//! chosen); ticket 8's `prepare-model` is the verified, content-addressed
//! install path.

use std::io::{Read, Write};
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use infra_embed::model::{self, EmbedConfig, Pooling};
use sha2::{Digest, Sha256};

#[derive(Parser)]
#[command(name = "infra-embed-eval")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Fetch {
        repo: String,
        revision: String,
        #[arg(long)]
        out: PathBuf,
    },
    Embed {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long, default_value = "mean")]
        pooling: String,
        #[arg(long)]
        corpus_prefix: Option<String>,
        #[arg(long)]
        query_prefix: Option<String>,
        #[arg(long, default_value_t = 384)]
        dims: usize,
        #[arg(long, default_value_t = 512)]
        max_tokens: usize,
        #[arg(long, default_value = "query")]
        side: String,
    },
}

fn sha256(path: &std::path::Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn fetch(repo: &str, revision: &str, out: &PathBuf) -> Result<(), String> {
    let api = hf_hub::api::sync::Api::new().map_err(|e| format!("hf api: {e}"))?;
    let repo_api = api.repo(hf_hub::Repo::with_revision(
        repo.to_string(),
        hf_hub::RepoType::Model,
        revision.to_string(),
    ));
    std::fs::create_dir_all(out).map_err(|e| format!("mkdir: {e}"))?;
    for fname in ["model.safetensors", "config.json", "tokenizer.json"] {
        let src = repo_api
            .get(fname)
            .map_err(|e| format!("get {fname}: {e}"))?;
        let dst = out.join(fname);
        std::fs::copy(&src, &dst).map_err(|e| format!("copy {fname}: {e}"))?;
        println!("{fname} sha256={}", sha256(&dst)?);
    }
    Ok(())
}

fn embed(args: &Cmd) -> Result<(), String> {
    let Cmd::Embed {
        dir,
        pooling,
        corpus_prefix,
        query_prefix,
        dims,
        max_tokens,
        side,
    } = args
    else {
        return Err("internal: embed called without Embed args".into());
    };
    let pooling = match pooling.as_str() {
        "mean" => Pooling::Mean,
        "cls" => Pooling::Cls,
        other => return Err(format!("unknown pooling: {other}")),
    };
    let is_query = match side.as_str() {
        "query" => true,
        "corpus" => false,
        other => return Err(format!("unknown --side {other:?}: expected query | corpus")),
    };
    let config = EmbedConfig {
        pooling,
        corpus_prefix: corpus_prefix.clone(),
        query_prefix: query_prefix.clone(),
        dims: *dims,
        max_input_tokens: *max_tokens,
    };
    let model = model::load(dir, config).map_err(|e| format!("load: {e}"))?;

    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("stdin: {e}"))?;
    let texts: Vec<String> = serde_json::from_str(&buf).map_err(|e| format!("json: {e}"))?;

    let vectors = model
        .embed_batch(&texts, is_query)
        .map_err(|e| format!("embed: {e}"))?;
    let out = serde_json::to_string(&vectors).map_err(|e| format!("serialize: {e}"))?;
    std::io::stdout()
        .write_all(out.as_bytes())
        .map_err(|e| format!("stdout: {e}"))?;
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let result = match &cli.cmd {
        Cmd::Fetch {
            repo,
            revision,
            out,
        } => fetch(repo, revision, out),
        cmd @ Cmd::Embed { .. } => embed(cmd),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
