# Privacy

This page documents every outbound data flow from a SentryUSB device,
the legal basis it relies on under GDPR, how long the data is retained,
and how to disable it. If anything you observe on the wire doesn't
match what's listed here, it's a bug — please open an issue.

## Summary

By default, SentryUSB sends **no device identifier** to our servers.
The "opt-in for analytics" toggle in the setup wizard and in
`Settings → System` is the only switch that controls whether a
device-derived identifier ever leaves your Pi.

## Per-flow disclosure

### 1. Daily update check

- **Endpoint:** `POST https://api.sentry-six.com/sentryusb/telemetry`
- **Sent:** `current_version`, `arch`, `model`, `update_available` flag,
  `new_version` (when relevant)
- **Identifier:** None by default. **If you have opted in** to the
  analytics toggle, a one-way salted SHA-256 of your board's serial
  number is included as `fingerprint`.
- **Purpose:** Detect vulnerable builds, ship compatible binaries,
  and (for opted-in devices) count unique installs without double-
  counting reinstalls.
- **Legal basis:** Legitimate interest under Art. 6(1)(f) for the
  default (no fingerprint) version — Recital 49 explicitly recognizes
  security as a legitimate-interest purpose. For the opted-in
  fingerprinted variant, consent under Art. 6(1)(a).
- **Retention:** Opted-in rows remain until they are manually purged or you
  request deletion. Opting out stops the fingerprint from being sent on
  future checks, but does not automatically delete an existing row.
  Non-fingerprinted calls are not stored — only rate-limit counters survive
  briefly in RAM.
- **How to disable:** `Settings → System → Analytics opt-in → Opted
  out`. The toggle immediately stops future identified checks. To request
  deletion of a previously stored analytics row, contact
  `privacy@sentry-six.com`.

### 2. Aggregate install beacon (no payload or device ID)

- **Endpoint:** `POST https://api.sentry-six.com/sentryusb/install-beacon`
- **Sent:** An empty request body with no custom identifier. As with any
  internet request, the server necessarily sees normal connection metadata,
  including the source IP.
- **Identifier:** No device or hardware fingerprint. The application uses the
  source IP only in a short-lived in-memory rate-limit bucket and persists
  only a daily aggregate count.
- **Purpose:** Tell us gross install volume independent of the opt-in
  cohort — i.e. so we can see if a release attracted new installs at
  all without knowing anything about anyone.
- **Legal basis:** Legitimate interests under Art. 6(1)(f) to measure
  aggregate install volume and protect the counter from abuse, balanced by
  sending no payload or device identifier and retaining no per-install row.
- **Retention:** Daily counts are kept indefinitely as aggregate
  numbers. The application rate-limit bucket remains only in memory for up
  to about two hours; no per-install application record is retained.
- **How to disable:** Fires exactly once per install (gated by a
  `/mutable/.beaconed` marker). To suppress entirely, create that file
  before first boot: `sudo touch /mutable/.beaconed`. Network-block
  `api.sentry-six.com` if you want to be sure.

### 3. Wraps / lock chime submissions

- **Endpoint:** `POST https://api.sentry-six.com/wraps/upload`,
  `POST https://api.sentry-six.com/lockchime/upload`
- **Sent:** The file you uploaded (plus an optional preview for wraps), display
  name, model and original filename for wraps, file size, audio duration for
  lock chimes, and your source IP. The server also generates a submission code
  and records review status and timestamps.
- **Identifier:** **No device fingerprint.** Older versions sent an
  `X-Fingerprint` header — that was removed. Abuse handling now goes
  through the Discord moderation queue plus per-IP rate limits.
- **Purpose:** Accepting, reviewing, and publishing your contribution, plus
  proportionate rate limiting, abuse investigation, and moderation.
- **Legal basis:** Contractual necessity under Art. 6(1)(b) for accepting,
  reviewing, and publishing the contribution you requested; legitimate
  interests under Art. 6(1)(f) for proportionate rate limiting, abuse
  investigation, and moderation.
- **Retention:** Pending and approved asset files remain until declined or
  manually deleted. Declining removes the asset file, but the submission row,
  including its source IP, status, and review metadata, has no fixed automatic
  deletion period and remains until manually removed or deletion is requested.
  The source IP is not exposed in the public library view.
- **How to disable:** Don't submit. Browsing/downloading the library
  sends no custom or device identifier; the source IP is necessarily seen
  and briefly used for rate limiting.

### 4. Wraps / lock chime downloads

- **Endpoint:** `GET https://api.sentry-six.com/wraps/download/<code>`,
  `GET https://api.sentry-six.com/lockchime/download/<code>`
- **Sent:** Standard HTTP request with no custom or device identifier. The
  source IP is necessarily seen and briefly used for rate limiting.
- **Identifier:** None is sent by current SentryUSB versions. Older versions
  may still send a legacy `X-Fingerprint` header; the current client does not.
- **Purpose:** Fetch the requested asset.
- **Legal basis:** Contractual necessity — you asked for the file.
- **Retention:** Current-client source-IP rate-limit entries remain only
  briefly in memory. Legacy per-asset fingerprint download records created by
  older clients may remain until manually deleted; contact
  `privacy@sentry-six.com` to request deletion.
- **How to disable:** Don't download.

### 5. Sentry Cloud (sync feature, opt-in)

- **Endpoint:** Various `https://api.sentry-six.com/cloud/...` routes.
- **Sent:** Your Sentry Cloud account credentials when signing in, followed by
  encrypted drive-history and telemetry data you choose to sync. Sentry Cloud
  does not upload dashcam video.
- **Identifier:** Your Sentry Cloud account.
- **Purpose:** Cloud sync requires it — the feature can't function
  otherwise.
- **Legal basis:** Contractual necessity (Art. 6(1)(b)) — you signed
  up for the service.
- **Retention:** See the Sentry Cloud terms of service.
- **How to disable:** Don't sign in to Cloud. The feature is fully
  opt-in.

### 6. iOS push notification pairing (opt-in)

- **Endpoint:** `POST https://notifications.sentry-six.com/register-code`
- **Sent:** A `device_id` (random UUID generated on this Pi),
  `device_secret`, your chosen pairing code, and your Pi's hostname.
- **Identifier:** The `device_id` — but it's a random value created
  locally on first run, **not** derived from your hardware. Resetting
  it generates a new one.
- **Purpose:** Routing push notifications from your Pi to your phone.
- **Legal basis:** Consent — you actively enabled this feature.
- **Retention:** Kept until you unpair the device.
- **How to disable:** Don't pair, or unpair in the iOS app + delete
  the credentials on the Pi.

### 7. AI Support & Help (user-initiated)

- **Endpoint:** Your browser talks only to this Pi at
  `/api/support/ai/...`. The Pi proxies the fixed request types to
  `https://api.sentry-six.com/ai-support/...`. The local browser-to-Pi hop
  normally uses HTTP, so chat content and access tokens are not encrypted on
  that local hop. Use AI Support only from a trusted local network. The Pi's
  onward connection to the public API uses HTTPS.
- **Sent when you start a conversation:** The text you enter, a random
  conversation/request identifier, the displayed disclosure version,
  the installed Sentry USB software version, and the fixed product ID
  `sentry-usb-rusty`. The Pi does **not** send a hardware fingerprint,
  Pi login credential, or knowledge for Dash USB or another product.
- **AI processing:** Relevant conversation content is processed by
  Ollama Cloud to generate a response. Ollama states that Cloud prompts
  and responses are processed transiently, are not retained beyond the
  request, and are not used to train models. Processing may occur outside
  Canada, including in the United States. The app identifies this as an
  online AI service; the model is not running solely on your Pi.
- **Server logging:** A redacted transcript of messages and responses,
  timestamps, product/version context, action decisions, and limited
  operational metadata is logged on Sentry Six servers. Authorized
  maintainers may review it to troubleshoot failures, find hallucinations,
  and improve support quality.
- **Identifier and local resume data:** To resume a chat, your browser stores
  the random conversation ID and raw random access token in local storage.
  The Sentry Six backend stores only a one-way hash of that token. The public
  IP seen from the Pi's connection is processed separately for rate limiting
  and abuse prevention: raw values remain only in short-lived in-memory rate
  buckets (up to about two hours), while a gateway-keyed one-way hash and
  daily diagnostic-upload counters may remain for up to about 49 hours and
  are not linked to the transcript or diagnostic text. None of these values
  is derived from your Pi hardware. Clearing browser
  site data removes local access but does not delete the server copy; use
  **New chat** first if you want the current conversation deleted immediately.
- **Purpose and legal basis:** Processing the message needed to answer your
  support request is necessary to provide the AI Support service you asked
  for (Art. 6(1)(b)). Bounded, redacted transcript retention and authorized
  review for security, reliability, hallucination detection, and prompt or
  knowledge-safety improvements rely on our legitimate interests
  (Art. 6(1)(f)). You may object to that processing or request deletion at
  `privacy@sentry-six.com`, subject to applicable law. The pre-chat screen is
  an acknowledgement of these disclosures, not bundled consent for every
  processing purpose.
- **Retention:** Redacted conversation transcripts expire 90 days after
  the last activity. You can delete the current conversation immediately
  in the UI, or request deletion at `privacy@sentry-six.com`. Deletion removes
  messages and uploaded files immediately. A non-content receipt containing
  the conversation ID, one-way hashes of the access token and deletion
  idempotency key, and deletion time remains available solely for safe retries
  and replay prevention until it expires 24 hours after deletion. The expired
  receipt is removed during the next scheduled cleanup sweep, normally within
  about one additional hour.
- **How to disable:** Do not start an AI Support conversation. The rest of
  SentryUSB continues to work without it.

#### Diagnostic-file requests

The assistant cannot browse the Pi or upload a file by itself. The first
supported request type is a predefined SentryUSB diagnostics report. It
collects the date, hostname, uptime, software/OS and board details, storage
and USB-gadget state, local network addresses, service state, temperatures,
and limited tails from designated system and SentryUSB logs, including recent
`archiveloop.log` entries. It does not search arbitrary files. Log excerpts can incidentally contain local
addresses, device or vehicle identifiers, error payloads, or location-related
details; those are not separate fixed fields requested from the vehicle.

If the assistant asks for the report, the UI shows the exact file, reason,
maximum size, destination, and seven-day retention period. Nothing is
generated or uploaded until you click **Generate & upload diagnostics once**
for that specific request. That click creates a short-lived, single-use
upload token; it is not standing permission for future files. You may deny
the request and continue chatting.

The approved report leaves your Pi, is uploaded to the Sentry Six backend,
and relevant content may be processed transiently by Ollama Cloud and
reviewed by authorized maintainers. The backend accepts only the expected
diagnostics request, plain UTF-8 text with control characters removed, and a
maximum of 2 MiB. The file is deleted automatically after seven days. Review
the disclosed categories before approving, and do not approve an upload if
the report may contain passwords, tokens, private keys, precise location, or
third-party data you are not authorized to share.

#### Optional Discord help

AI Support may suggest the Sentry Six Discord for community or human help.
Opening the Discord link does not automatically send the AI transcript or
uploaded diagnostics to Discord. Anything you choose to post there is a
separate disclosure to Discord and is governed by Discord's policies.

## Things SentryUSB does **not** do

- Send a hardware fingerprint without explicit opt-in.
- Phone home on every boot. (The old `spawn_startup_telemetry` was
  removed entirely in the privacy overhaul.)
- Send "diagnostics" or "crash reports" in the background. If a crash
  reporter is ever added, it will be its own opt-in.
- Let AI Support browse files or treat one approval as permission for a
  later upload.
- Let the Rusty UI choose another product's AI prompt or knowledge base.
- Bundle optional consents under one button. Each optional consent, including
  a diagnostic-file upload, requires its own affirmative action; acknowledging
  the pre-chat disclosure is not bundled consent for security or quality review.
- Use pre-ticked checkboxes — explicit click required.

## Source code references

If you want to verify any of the above against the source:

- Update-check telemetry: `crates/api/src/update.rs` → `send_telemetry()`.
  Look for the `analytics_opt_in` read and confirm the `fingerprint`
  key is only inserted when that pref is `true`.
- Install beacon: same file → `spawn_install_beacon()`. The POST is
  bodyless.
- Wraps/chimes header forwarding: `crates/api/src/community.rs` →
  `forward_headers()`. Should only forward `x-passcode`, never
  `x-fingerprint`.
- Notification pairing: `crates/api/src/notifications.rs` →
  `register_code_with_backend()`. Confirm the request body has no
  `fingerprint` field.

- AI Support proxy and product lock: `crates/api/src/support.rs`. Confirm
  `AI_PRODUCT_ID` is `sentry-usb-rusty`, later requests remove product
  selectors, only the browser-supplied AI conversation token and idempotency
  key are forwarded, and the trusted product header is injected by the Pi.
- AI Support disclosure acknowledgement and separate upload consent:
  `web/src/components/support/AISupportChat.tsx` and
  `web/src/api/support.ts`.

## Reporting a privacy bug

Open an issue at
[github.com/Sentry-Six/Sentry-USB-Rusty/issues](https://github.com/Sentry-Six/Sentry-USB-Rusty/issues)
or email `privacy@sentry-six.com`. If the bug is "the client sent X
even though the docs said it wouldn't" please include a `tcpdump` or
the relevant journalctl line so we can fix it.
