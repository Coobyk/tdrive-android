//! Personal-drive encryption — vault state, unlock, first-time setup.
use super::*;

impl Worker {
    pub(super) fn emit_vault_state(&self) {
        let configured = self.namespace.encfg.is_some();
        let hint = self
            .namespace
            .encfg
            .as_ref()
            .map(|o| o.hint.clone())
            .unwrap_or_default();
        self.emit(Event::VaultState {
            configured,
            unlocked: self.master_key.lock().unwrap().is_some(),
            hint,
        });
    }

    pub(super) async fn unlock(&mut self, password: &str) -> Result<()> {
        let op = self
            .namespace
            .encfg
            .clone()
            .context("drive has no encryption vault")?;
        let vault = VaultConfig::from_op(
            &op.kdf_salt_b64,
            &op.kdf_params_json,
            &op.wrapped_key_b64,
            &op.key_check_b64,
            &op.hint,
            op.config_version,
        )?;
        let key = tokio::task::spawn_blocking({
            let vault = vault.clone();
            let password = password.to_string();
            move || vault.unlock(password.as_bytes())
        })
        .await??;
        *self.master_key.lock().unwrap() = Some(key);
        self.emit_vault_state();
        Ok(())
    }

    pub(super) async fn enable_encryption(&mut self, password: &str, hint: &str) -> Result<()> {
        if !self.channel_kind.is_personal() {
            return Err(anyhow!("encryption is only supported on personal drives"));
        }
        if self.namespace.encfg.is_some() {
            return Err(anyhow!("encryption already configured"));
        }
        let password = password.to_string();
        let hint = hint.to_string();
        let (vault, master) =
            tokio::task::spawn_blocking(move || crypto::create_vault(password.as_bytes(), &hint))
                .await??;
        let op = ops::enc_config(&vault);
        self.send_and_project(&op).await?;
        *self.master_key.lock().unwrap() = Some(master);
        self.emit_vault_state();
        Ok(())
    }
}
