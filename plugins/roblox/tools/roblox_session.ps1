# What a Roblox session on the real-AOSP path is, for the scripts here that run the r_app test
# binary directly (no launcher, so no plugin hook): the same variables the roblox plugin's
# session-start hook names (plugins/roblox/src/main.rs, `session_env`). Dot-source it, then set
# OMNI_R_LINK_ID to the place id.
$env:OMNI_R_LINK = '-a android.intent.action.VIEW -d "roblox://experiences/start?placeId={id}" -n com.roblox.client/com.roblox.client.ActivityProtocolLaunch'
$env:OMNI_R_LINK_READY = 'com.roblox.client/.ActivityNativeMain}'
$env:OMNI_R_SIGNED_IN = 'DID_LOG_IN'
$env:OMNI_R_LINK_TAKEN = 'Joining game'
$env:OMNI_R_LOADED = 'onGameLoaded() SessionReporterState_GameLoaded placeId:'
$env:OMNI_R_DISCONNECTED = 'Client has been disconnected'
if (-not $env:OMNI_R_LOCALE) { $env:OMNI_R_LOCALE = 'tr-TR' }
