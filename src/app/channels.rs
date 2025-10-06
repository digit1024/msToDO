use tokio::sync::mpsc;
use std::sync::Arc;
use tracing::{info, error};

#[derive(Debug, Clone)]
pub enum CacheUpdateSignal {
    ListTasksUpdated(String), // list_id
    TaskUpdated(String, String), // task_id, list_id
    TaskAdded(String, String), // task_id, list_id
    TaskRemoved(String, String), // task_id, list_id
}

/// Centralized channel manager for the application
#[derive(Debug)]
pub struct ChannelManager {
    cache_update_sender: mpsc::UnboundedSender<CacheUpdateSignal>,
    cache_update_receiver: mpsc::UnboundedReceiver<CacheUpdateSignal>,
}

impl ChannelManager {
    /// Create a new channel manager with all necessary channels
    pub fn new() -> Self {
        let (cache_update_sender, cache_update_receiver) = mpsc::unbounded_channel();
        
        info!("🔗 Channel manager initialized");
        
        Self {
            cache_update_sender,
            cache_update_receiver,
        }
    }
    
    /// Get the cache update sender (for storage layer)
    pub fn cache_update_sender(&self) -> mpsc::UnboundedSender<CacheUpdateSignal> {
        self.cache_update_sender.clone()
    }
    
    /// Get the cache update receiver (for UI layer)
    pub fn cache_update_receiver(&mut self) -> &mut mpsc::UnboundedReceiver<CacheUpdateSignal> {
        &mut self.cache_update_receiver
    }
    
    /// Send a cache update signal
    pub fn send_cache_update(&self, signal: CacheUpdateSignal) {
        if let Err(e) = self.cache_update_sender.send(signal) {
            error!("❌ Failed to send cache update signal: {}", e);
        }
    }
}

/// Global channel manager instance
static CHANNEL_MANAGER: std::sync::OnceLock<Arc<ChannelManager>> = std::sync::OnceLock::new();

/// Initialize the global channel manager
pub fn init_channels() -> Arc<ChannelManager> {
    CHANNEL_MANAGER.get_or_init(|| {
        Arc::new(ChannelManager::new())
    }).clone()
}

/// Get the global channel manager
pub fn get_channel_manager() -> Arc<ChannelManager> {
    CHANNEL_MANAGER.get().expect("Channel manager not initialized").clone()
}
