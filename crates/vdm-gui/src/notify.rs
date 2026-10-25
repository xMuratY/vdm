//! Desktop notifications ("Download complete").

/// Show a notification without blocking the UI. Failures are only logged:
/// a missing notification service must never break a download.
pub fn notify(summary: impl Into<String>, body: impl Into<String>) {
    let (summary, body) = (summary.into(), body.into());
    std::thread::spawn(move || {
        let shown = notify_rust::Notification::new()
            .appname("Vibe Download Manager")
            .summary(&summary)
            .body(&body)
            .show();
        if let Err(err) = shown {
            tracing::warn!(%err, "could not show a notification");
        }
    });
}
