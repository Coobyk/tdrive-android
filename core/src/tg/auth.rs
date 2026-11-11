//! Login and session lifecycle — connect, code/2FA submit, sign-out.
use super::*;

impl Worker {
    pub(super) async fn connect(&mut self, api_id: i32, api_hash: String) -> Result<()> {
        let session_path = crate::paths::session_path()?;
        let session = SqliteSession::open(session_path).await?;
        let pool = SenderPool::with_configuration(
            Arc::new(session),
            api_id,
            grammers_client::sender::ConnectionParams {
                retry_policy: Box::new(FloodSleep {
                    tx: self.tx.clone(),
                }),
                ..Default::default()
            },
        );
        let client = Client::new(pool.handle.clone());
        let mut updates = pool.updates;
        tokio::spawn(pool.runner.run());
        // Any incoming update hints at new history — trigger a (cheap)
        // incremental sync back through the command queue.
        let cmd_tx = self.cmd_tx.clone();
        let queued = self.live_sync_queued.clone();
        tokio::spawn(async move {
            while updates.recv().await.is_some() {
                // Coalesce — a chatty account must not queue a resync
                // backlog that starves user commands.
                if !queued.swap(true, std::sync::atomic::Ordering::Relaxed)
                    && cmd_tx.send(Command::LiveSync).is_err()
                {
                    break;
                }
            }
        });
        self.api_hash = api_hash;
        self.client = Some(client);
        let authorized = self.client()?.is_authorized().await.unwrap_or(false);
        self.emit(Event::Connected { authorized });
        Ok(())
    }

    pub(super) async fn request_code(&mut self, phone: String) -> Result<()> {
        let token = self
            .client()?
            .request_login_code(&phone, &self.api_hash)
            .await
            .map_err(|e| anyhow!("request code: {e}"))?;
        self.login_token = Some(token);
        self.emit(Event::CodeSent);
        Ok(())
    }

    pub(super) async fn submit_code(&mut self, code: String) -> Result<()> {
        let token = self.login_token.as_ref().context("no login in progress")?;
        match self.client()?.sign_in(token, &code).await {
            Ok(user) => {
                self.emit(Event::SignedIn {
                    name: user.full_name(),
                });
                self.after_login().await?;
            }
            Err(SignInError::PasswordRequired(token)) => {
                let hint = token.hint().unwrap_or("").to_string();
                self.password_token = Some(token);
                self.emit(Event::NeedPassword { hint });
            }
            Err(SignInError::InvalidCode) => {
                return Err(anyhow!("invalid login code"));
            }
            Err(SignInError::InvalidPassword(token)) => {
                self.password_token = Some(token);
                return Err(anyhow!("invalid 2FA password"));
            }
            Err(SignInError::SignUpRequired) => {
                return Err(anyhow!("this phone number has no Telegram account"));
            }
            Err(SignInError::Other(e)) => return Err(anyhow!("sign in: {e}")),
        }
        Ok(())
    }

    pub(super) async fn submit_password(&mut self, password: String) -> Result<()> {
        let token = self
            .password_token
            .take()
            .context("no password requested")?;
        match self.client()?.check_password(token, password).await {
            Ok(user) => {
                self.emit(Event::SignedIn {
                    name: user.full_name(),
                });
                self.after_login().await?;
            }
            Err(SignInError::InvalidPassword(token)) => {
                self.password_token = Some(token);
                return Err(anyhow!("invalid 2FA password"));
            }
            Err(e) => return Err(anyhow!("password check: {e}")),
        }
        Ok(())
    }

    pub(super) async fn after_login(&mut self) -> Result<()> {
        self.login_token = None;
        self.password_token = None;
        if let Ok(me) = self.client()?.get_me().await {
            self.actor_id = me.id().bare_id_unchecked();
        }
        self.list_channels().await
    }

    pub(super) async fn sign_out(&mut self) -> Result<()> {
        if let Some(client) = &self.client {
            let _ = client.sign_out().await;
        }
        // Keep the client — grammers supports re-login on the same pool;
        // dropping it strands the worker with no path back.
        for (_, token) in self.cancels.drain() {
            token.cancel();
        }
        self.paused_downloads.clear();
        *self.peer_cell.lock().unwrap() = None;
        *self.ns_shared.lock().unwrap() = Arc::new(Namespace::new());
        self.channel_id = 0;
        self.channel_kind = DriveKind::Personal;
        self.login_token = None;
        self.password_token = None;
        *self.master_key.lock().unwrap() = None;
        self.namespace = Arc::new(Namespace::new());
        self.emit(Event::SignedOut);
        Ok(())
    }
}
