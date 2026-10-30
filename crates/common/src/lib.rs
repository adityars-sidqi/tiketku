//! Infrastruktur bersama untuk semua service TiketKu.
//!
//! Crate ini hanya berisi infrastruktur (config, tracing, error problem+json,
//! health, shutdown), bukan model domain, supaya service tidak saling terikat.
//! Isinya diisi bertahap mulai story #3 dan #4.

/// Pesan yang dicetak service saat start, mis. `auth-service v0.1.0 berjalan`.
pub fn startup_message(service: &str, version: &str) -> String {
    format!("{service} v{version} berjalan")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_message_memuat_nama_dan_versi() {
        assert_eq!(
            startup_message("auth-service", "0.1.0"),
            "auth-service v0.1.0 berjalan"
        );
    }
}
