use crate::{backend::Backend,config::{self,Profile,Protocol},transfer::TransferContext};
use anyhow::Result;
use std::sync::Arc;
pub async fn connect(profile:&Profile,name:&str,context:TransferContext)->Result<Arc<dyn Backend>> {
    config::validate_profile(profile)?;let password=config::password(profile,name)?;
    match profile.protocol {
        Protocol::Webdav=>Ok(Arc::new(crate::webdav::WebDav::new(profile,&password)?.with_lock_token(std::env::var("RDRIVE_WEBDAV_LOCK_TOKEN").ok())?.with_transfer_context(context))),
        Protocol::Smb=>Ok(Arc::new(crate::smb::Smb::new(profile,&password).await?.with_transfer_context(context))),
    }
}
