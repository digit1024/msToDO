mod app;
mod core;
mod pages;
mod storage;
mod auth;
mod integration;

use core::settings;
use auth::MsTodoAuth;
use tracing::{info, error};

pub use app::error::*;



pub fn main() -> cosmic::iced::Result {
    // Initialize settings
    settings::app::init();
    
    info!("🚀 Starting Tasks app with Microsoft Todo integration...");
    
    // Check authentication first
    let auth = match MsTodoAuth::new() {
        Ok(auth) => auth,
        Err(e) => {
            error!("❌ Failed to initialize authentication: {}", e);
            return cosmic::app::run::<settings::error::View>(
                settings::app::settings(),
                settings::error::flags(crate::LocalStorageError::AuthenticationFailed(e.to_string())),
            );
        }
    };
    
    // Check if we can get a valid access token (with automatic refresh)
    info!("🔍 Checking for valid access token...");
    match auth.get_access_token() {
        Ok(_token) => {
            info!("✅ Valid access token obtained (refreshed if needed), proceeding to main app...");
        }
        Err(_e) => {
            info!("🔐 No valid tokens found or refresh failed, starting authentication flow...");
            
            match auth.authorize() {
                Ok(config) => {
                    info!("✅ Authentication successful! Token expires at: {}", config.expires_at);
                }
                Err(e) => {
                    error!("❌ Authentication failed: {}", e);
                    return cosmic::app::run::<settings::error::View>(
                        settings::app::settings(),
                        settings::error::flags(crate::LocalStorageError::AuthenticationFailed(format!("Authentication failed: {}", e))),
                    );
                }
            }
        }
    }
    
    // Now proceed with the normal app flow
    match settings::app::storage() {
        Ok(storage) => {
            cosmic::app::run::<app::TasksApp>(settings::app::settings(), app::flags(storage))
        }
        Err(error) => cosmic::app::run::<settings::error::View>(
            settings::app::settings(),
            settings::error::flags(error),
        ),
    }
}
