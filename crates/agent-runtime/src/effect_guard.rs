//! The observer contains only cancellation state. Cloning it does not retain
//! source read leases; the non-cloneable guard follows the actual worker.
#[derive(Clone)]
pub(crate) struct EffectObserver(std::sync::Arc<dyn Fn() -> bool + Send + Sync>);
impl EffectObserver {
    pub(crate) fn is_live(&self) -> bool {
        (self.0)()
    }
    pub(crate) async fn revoked(&self) {
        while self.is_live() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}
#[cfg(test)]
type CheckpointHook = Option<(&'static str, Box<dyn FnOnce() + Send>)>;
pub(crate) struct EffectGuard {
    observer: EffectObserver,
    _lifetime: Box<dyn std::any::Any + Send>,
    #[cfg(test)]
    hook: std::sync::Mutex<CheckpointHook>,
}
impl EffectGuard {
    pub(crate) fn new(
        live: impl Fn() -> bool + Send + Sync + 'static,
        lifetime: impl Send + 'static,
    ) -> Self {
        Self {
            observer: EffectObserver(std::sync::Arc::new(live)),
            _lifetime: Box::new(lifetime),
            #[cfg(test)]
            hook: std::sync::Mutex::new(None),
        }
    }
    pub(crate) fn observer(&self) -> EffectObserver {
        self.observer.clone()
    }
    pub(crate) fn check(&self, point: &'static str) -> bool {
        #[cfg(test)]
        {
            let hook = {
                let mut h = self.hook.lock().expect("hook");
                if h.as_ref().is_some_and(|(p, _)| *p == point) {
                    h.take()
                } else {
                    None
                }
            };
            if let Some((_, hook)) = hook {
                hook();
            }
        }
        let _ = point;
        self.observer.is_live()
    }
    #[cfg(test)]
    pub(crate) fn before_for_test(
        &self,
        point: &'static str,
        hook: impl FnOnce() + Send + 'static,
    ) {
        *self.hook.lock().expect("hook") = Some((point, Box::new(hook)));
    }
}
