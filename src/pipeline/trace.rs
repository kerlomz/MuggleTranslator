use std::path::{Path, PathBuf};

use anyhow::Context;

pub struct TraceWriter {
    dir: PathBuf,
    enabled: bool,
}

impl TraceWriter {
    pub fn new(dir: PathBuf, enabled: bool) -> anyhow::Result<Self> {
        if enabled {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("create trace dir: {}", dir.display()))?;
        }
        Ok(Self { dir, enabled })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn write_named_text(&self, name: &str, text: &str) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let path = self.dir.join(sanitize_filename(name));
        std::fs::write(&path, text).with_context(|| format!("write trace: {}", path.display()))?;
        Ok(())
    }

    pub fn write_tu_text(
        &self,
        tu_id: usize,
        stage: &str,
        kind: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let name = format!("tu_{tu_id:06}.{stage}.{kind}.txt");
        self.write_named_text(&name, text)
    }
}

fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            _ => out.push(ch),
        }
    }
    out
}

