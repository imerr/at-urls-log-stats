use serde::Deserialize;
use std::fs::File;

#[derive(Deserialize)]
#[serde(default = "Config::default")]
pub struct Config {
    /// This specifies which docker containers to track
    pub docker_images: Vec<String>,
    pub docker_socket: String,
    pub listen_address: String,
}

impl Config {
    pub fn default() -> Config {
        Config {
            docker_images: vec!["atdr.meo.ws/archiveteam/urls-grab".to_string()],
            docker_socket: "/var/run/docker.sock".to_string(),
            listen_address: "0.0.0.0:8000".to_string(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.docker_images.is_empty() {
            return Err("docker_images can't be empty".to_string());
        }

        if self.docker_socket.is_empty() {
            return Err("docker_socket can't be empty".to_string());
        }

        Ok(())
    }

    pub fn load_from_file(path: &str) -> Result<Self, String> {
        match File::open(path) {
            Ok(f) => match serde_json::from_reader::<_, Self>(f) {
                Ok(c) => match c.validate() {
                    Ok(_) => Ok(c),
                    Err(e) => Err(format!("Config failed to validate: {e}")),
                },
                Err(e) => Err(format!("Failed to read config from 'config.json': {}", e)),
            },
            Err(e) => Err(format!("Failed to open 'config.json' for reading: {}", e)),
        }
    }
}
