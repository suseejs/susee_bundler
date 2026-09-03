use colored::*;
use napi_derive::napi;
use serde::{Deserialize, Serialize};

/// Susee read and write file
#[napi]
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuseeFs;

#[napi]
impl SuseeFs {
    /// read file to string
    #[napi]
    pub fn read_file(file_path: String) -> std::string::String {
        let root = std::env::current_dir().unwrap_or_default();
        let abs = root.join(&file_path);
        if std::fs::exists(&abs).is_err() {
            eprintln!(
                "{} [{}] {}",
                "File".magenta(),
                file_path.cyan(),
                "dose not exists.".magenta()
            );
            std::process::exit(1);
        }
        let error_text = format!("Error when reading {file_path}");
        let content = std::fs::read_to_string(&abs).expect(&error_text.magenta());
        content
    }
    /// write content to a file
    #[napi]
    pub fn write_file(file_path: String, content: String) -> napi::Result<()> {
        let p = std::path::Path::new(&file_path);
        if let Some(parent) = p.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        }
        std::fs::write(p, content).map_err(|e| napi::Error::from_reason(e.to_string()))
    }
}
