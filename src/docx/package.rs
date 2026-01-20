use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::Context;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub struct DocxPackage {
    pub entries: Vec<DocxEntry>,
}

pub struct DocxEntry {
    pub name: String,
    pub data: Vec<u8>,
    pub compression: CompressionMethod,
    pub last_modified: zip::DateTime,
    pub unix_mode: Option<u32>,
    pub is_dir: bool,
}

impl DocxPackage {
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let f = File::open(path).with_context(|| format!("open docx: {}", path.display()))?;
        let mut zip = ZipArchive::new(f).context("read zip")?;
        let mut entries = Vec::new();
        for i in 0..zip.len() {
            let mut file = zip.by_index(i).context("zip entry")?;
            let mut data = Vec::with_capacity(file.size() as usize);
            file.read_to_end(&mut data).context("read zip entry")?;
            entries.push(DocxEntry {
                name: file.name().to_string(),
                data,
                compression: file.compression(),
                last_modified: file.last_modified().unwrap_or_default(),
                unix_mode: file.unix_mode(),
                is_dir: file.is_dir(),
            });
        }
        Ok(Self { entries })
    }

    pub fn write_with_replacements(
        &self,
        output_path: &Path,
        replacements: &HashMap<String, Vec<u8>>,
    ) -> anyhow::Result<()> {
        let f = File::create(output_path)
            .with_context(|| format!("create output docx: {}", output_path.display()))?;
        let mut zout = ZipWriter::new(f);
        for ent in &self.entries {
            let data = replacements
                .get(&ent.name)
                .cloned()
                .unwrap_or_else(|| ent.data.clone());
            let mut opts = SimpleFileOptions::default()
                .compression_method(ent.compression)
                .last_modified_time(ent.last_modified);
            if let Some(mode) = ent.unix_mode {
                opts = opts.unix_permissions(mode);
            }
            if ent.is_dir || ent.name.ends_with('/') {
                zout.add_directory(&ent.name, opts)
                    .with_context(|| format!("add zip dir: {}", ent.name))?;
            } else {
                zout.start_file(&ent.name, opts)
                    .with_context(|| format!("start zip file: {}", ent.name))?;
                zout.write_all(&data)
                    .with_context(|| format!("write zip file: {}", ent.name))?;
            }
        }
        zout.finish().context("finish zip")?;
        Ok(())
    }

    pub fn xml_entries(&self) -> Vec<&DocxEntry> {
        self.entries
            .iter()
            .filter(|e| e.name.to_lowercase().ends_with(".xml"))
            .collect()
    }
}
