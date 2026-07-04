// Goopie Launcher – synchronous native bridge shim
// Injected as an initialization_script before any page JavaScript runs.
// __BRIDGE_BASE__ and __BRIDGE_TOKEN__ are substituted at runtime by make_init_script().
(function () {
  'use strict';

  // __BRIDGE_BASE__ is substituted at runtime by make_init_script():
  //   Linux/macOS: goopiebridge://localhost/bridge/
  //   Windows:     http://goopiebridge.localhost/bridge/
  // Using a custom URI scheme (registered as secure by wry) avoids mixed-content
  // blocking when the page is served over HTTPS.
  var BASE = '__BRIDGE_BASE__';
  var TOKEN = '__BRIDGE_TOKEN__';

  /**
   * Make a synchronous call to the Rust bridge.
   * @param {string} fn_name  The global function name (matches the Rust dispatch key).
   * @param {Array}  args     Arguments array, JSON-serialised and sent as the `args` query param.
   * @returns The parsed JSON response body, or null on error.
   */
  function call(fn_name, args) {
    var xhr = new XMLHttpRequest();
    var encoded = encodeURIComponent(JSON.stringify(args));
    var url = BASE + encodeURIComponent(fn_name) + '?token=' + TOKEN + '&args=' + encoded;
    try {
      xhr.open('GET', url, false /* synchronous */);
      xhr.send(null);
      if (xhr.status === 200) {
        return JSON.parse(xhr.responseText);
      }
      console.warn('[GoopieLauncher] bridge error for ' + fn_name + ': HTTP ' + xhr.status + ' – ' + xhr.responseText);
      return null;
    } catch (e) {
      console.warn('[GoopieLauncher] bridge XHR exception for ' + fn_name + ':', e);
      return null;
    }
  }

  // ── Platform ─────────────────────────────────────────────────────────────────
  window.GetPlatform     = function ()    { return call('GetPlatform', []); };
  window.GetArch         = function ()    { return call('GetArch', []); };
  window.getVersion      = function ()    { return call('getVersion', []); };

  // ── Launcher self-update ─────────────────────────────────────────────────────
  // `CheckForLauncherUpdate` is a cheap synchronous read of the cache kept
  // fresh by `launcher::spawn_update_monitor`; `SelfUpdateLauncher` kicks off
  // the download/apply (progress polled via getLauncherUpdateProgress/-String
  // below — kept separate from getDownloadProgress/-String, which track a
  // game's own download, so the two never light up each other's UI).
  window.CheckForLauncherUpdate = function ()  { return call('CheckForLauncherUpdate', []); };
  window.SelfUpdateLauncher     = function ()  { return call('SelfUpdateLauncher', []); };
  window.isLauncherUpdating              = function ()  { return call('isLauncherUpdating', []); };
  window.getLauncherUpdateProgress       = function ()  { return call('getLauncherUpdateProgress', []); };
  window.getLauncherUpdateProgressString = function ()  { return call('getLauncherUpdateProgressString', []); };

  // ── Config / paths ───────────────────────────────────────────────────────────
  window.GetGamesPath    = function ()    { return call('GetGamesPath', []); };
  window.SetGamesPath    = function ()    { return call('SetGamesPath', []); };
  window.GetLanguage     = function ()    { return call('GetLanguage', []); };
  window.SetLanguage     = function (l)   { return call('SetLanguage', [l]); };

  // ── Discord Rich Presence ────────────────────────────────────────────────────
  window.getDiscordPresenceEnabled = function ()    { return call('getDiscordPresenceEnabled', []); };
  window.setDiscordPresenceEnabled = function (v)   { return call('setDiscordPresenceEnabled', [v]); };

  // ── Proton (Linux) ───────────────────────────────────────────────────────────
  // On non-Linux launchers these commands are present (bridge always compiles
  // them) but getProtonInstallations returns [] and the rest are no-ops.
  // The website gates the UI on GetPlatform() === 'Linux' so they are never
  // called unnecessarily on other platforms.
  window.getProtonInstallations = function ()    { return call('getProtonInstallations', []); };
  window.getUseProton           = function ()    { return call('getUseProton', []); };
  window.setUseProton           = function (v)   { return call('setUseProton', [v]); };
  window.getSelectedProton      = function ()    { return call('getSelectedProton', []); };
  window.setSelectedProton      = function (p)   { return call('setSelectedProton', [p]); };

  // ── Game state ───────────────────────────────────────────────────────────────
  window.isIsoInstalled      = function (g)            { return call('isIsoInstalled', [g]); };
  window.isExeUpdated        = function (g, b)         { return call('isExeUpdated', [g, b]); };
  // `build` (below) identifies an installed build by its on-disk key — i.e.
  // the `name` field returned by getInstalledBuilds(g), which equals the
  // sanitised release tag the build was installed under.
  window.getInstalledVersion = function (g, b)         { return call('getInstalledVersion', [g, b]); };
  window.getInstalledBuilds  = function (g)            { return call('getInstalledBuilds', [g]); };

  // ── Long-running ops (fire-and-forget; poll for progress) ────────────────────
  window.Install         = function (g, x, s)          { return call('Install', [g, x, s]); };
  window.Uninstall       = function (g, b)             { return call('Uninstall', [g, b]); };
  window.UninstallAll    = function (g)                { return call('UninstallAll', [g]); };
  window.RemoveAssets    = function (g)                { return call('RemoveAssets', [g]); };
  window.Update          = function (g, u, a, v, p)    { return call('Update', [g, u, a, v, p]); };
  window.NeedsUpdate     = function (g, b, u, a)       { return call('NeedsUpdate', [g, b, u, a]); };
  window.Play            = function (g, b, c, e, r, m) { return call('Play', [g, b, c, e, r, m]); };
  // Running-game tracking: poll to drive the Play/Close button and the
  // "close the running game to start this one?" confirmation prompt.
  window.isGameRunning   = function ()    { return call('isGameRunning', []); };
  window.getRunningGame  = function ()    { return call('getRunningGame', []); };
  window.closeGame       = function ()    { return call('closeGame', []); };
  // Pollable error from the most recent Play attempt (e.g. executable not
  // found — likely an incompatible-platform build — or a spawn error).
  // Returns null when there is no pending error.
  window.getLaunchError   = function ()   { return call('getLaunchError', []); };
  window.clearLaunchError = function ()   { return call('clearLaunchError', []); };
  // Local-only per-game play-time total (never synced to the cloud). Returns
  // null if the game has never been played.
  window.getPlaytime = function (g) { return call('getPlaytime', [g]); };
  window.InstallPackage  = function (g, b, p, z, h, e) { return call('InstallPackage', [g, b, p, z, h, e]); };
  window.IsPackageInstalled = function (g, b, z)       { return call('IsPackageInstalled', [g, b, z]); };

  // ── Update & DLC management ──────────────────────────────────────────────────
  window.InstallAssetFile  = function (g, p, c, d, u, s) { return call('InstallAssetFile', [g, p, c, d, u, s]); };
  window.InstallAssetPick  = function (g, c, d, x, u, s) { return call('InstallAssetPick', [g, c, d, x, u, s]); };
  window.InstallAssetFiles = function (g, ps, c, d, u, s){ return call('InstallAssetFiles', [g, ps, c, d, u, s]); };
  window.isUpdateInstalled = function (g)          { return call('isUpdateInstalled', [g]); };
  window.RemoveUpdate      = function (g)          { return call('RemoveUpdate', [g]); };
  window.openUpdateFolder  = function (g)          { return call('openUpdateFolder', [g]); };
  window.getInstalledDlc   = function (g)          { return call('getInstalledDlc', [g]); };
  window.RemoveDlc         = function (g, t, h)    { return call('RemoveDlc', [g, t, h]); };
  window.openDlcFolder     = function (g, t, h)    { return call('openDlcFolder', [g, t, h]); };
  window.openBuildLogsFolder = function (g, b)     { return call('openBuildLogsFolder', [g, b]); };

  // ── Mods ──────────────────────────────────────────────────────────────────
  // installModArchives/pickModArchives are fire-and-forget (extraction runs
  // on a background thread) — poll isInstallingMods, then read getModInstallReport.
  window.getMods            = function (g)         { return call('getMods', [g]); };
  window.setModsState       = function (g, mods)   { return call('setModsState', [g, mods]); };
  window.installModArchives = function (g, p)      { return call('installModArchives', [g, p]); };
  window.pickModArchives    = function (g)         { return call('pickModArchives', [g]); };
  window.isInstallingMods   = function ()          { return call('isInstallingMods', []); };
  window.getModInstallReport = function ()         { return call('getModInstallReport', []); };
  window.removeMod          = function (g, id)     { return call('removeMod', [g, id]); };
  window.openModsFolder     = function (g)         { return call('openModsFolder', [g]); };
  // Validates the *enabled* mod set against requires/conflicts/load_after and
  // per-platform code-mod availability. `Play` itself already enforces this
  // (see getLaunchError) -- this is for the Mods panel to show the same
  // reasons proactively, before the player even tries to launch.
  window.getModValidation   = function (g)         { return call('getModValidation', [g]); };
  window.autoSortMods       = function (g)         { return call('autoSortMods', [g]); };

  // ── Global drag-and-drop (catalogue-wide matching) ───────────────────────────
  // `catalogue` is a JSON string (or array) of trimmed Game entries — see
  // extract::drop::CatalogueEntry for the expected fields. `focused` is the
  // recompName of whatever game page is currently focused, or '' for none.
  // Fire-and-forget; poll isExtracting/getDropStatus, then read getDropReport.
  window.ProcessDrops  = function (paths, focused, catalogue) { return call('ProcessDrops', [paths, focused, catalogue]); };
  window.getDropReport = function ()                          { return call('getDropReport', []); };
  window.getDropStatus = function ()                          { return call('getDropStatus', []); };

  // ── Progress polling ─────────────────────────────────────────────────────────
  window.isExtracting        = function ()    { return call('isExtracting', []); };
  window.isUpdating          = function ()    { return call('isUpdating', []); };
  window.getDownloadProgress = function ()    { return call('getDownloadProgress', []); };
  window.getDownloadString   = function ()    { return call('getDownloadString', []); };
  window.getExtractProgress  = function ()    { return call('getExtractProgress', []); };
  window.getExtractError     = function ()    { return call('getExtractError', []); };
  window.clearExtractError   = function ()    { return call('clearExtractError', []); };

  // ── Folder operations ────────────────────────────────────────────────────────
  window.OpenGamesFolder  = function ()       { return call('OpenGamesFolder', []); };
  window.openSaveFolder   = function (g)      { return call('openSaveFolder', [g]); };
  window.OpenExternalLink = function (url)    { return call('OpenExternalLink', [url]); };

  // ── Shortcuts ───────────────────────────────────────────────────────────────
  window.desktopShortcutExists  = function (g, t)    { return call('desktopShortcutExists', [g, t]); };
  window.appShortcutExists      = function (g, t)    { return call('appShortcutExists', [g, t]); };
  window.CreateDesktopShortcut  = function (g, t, i) { return call('CreateDesktopShortcut', [g, t, i]); };
  window.CreateAppShortcut      = function (g, t, i) { return call('CreateAppShortcut', [g, t, i]); };
  window.RemoveDesktopShortcut  = function (g, t)    { return call('RemoveDesktopShortcut', [g, t]); };
  window.RemoveAppShortcut      = function (g, t)    { return call('RemoveAppShortcut', [g, t]); };
  window.getAutoPlayGame   = function ()        { return call('getAutoPlayGame', []); };
  window.clearAutoPlayGame = function ()        { return call('clearAutoPlayGame', []); };

  // ── Save management ──────────────────────────────────────────────────────────
  window.getSaveSlots         = function (g)       { return call('getSaveSlots', [g]); };
  window.getSaveSlotCount     = function (g)       { return call('getSaveSlotCount', [g]); };
  window.getActiveSave        = function (g)       { return call('getActiveSave', [g]); };
  window.backupSave           = function (g, s)    { return call('backupSave', [g, s]); };
  window.restoreSave          = function (g, s)    { return call('restoreSave', [g, s]); };
  window.deleteSave           = function (g, s)    { return call('deleteSave', [g, s]); };
  window.renameSave           = function (g, o, n) { return call('renameSave', [g, o, n]); };
  window.deleteCurrentSave    = function (g)       { return call('deleteCurrentSave', [g]); };

  // ── Achievements ─────────────────────────────────────────────────────────────
  window.getAchievements       = function (g) { return call('getAchievements', [g]); };
  window.getAchievementSummary = function (g) { return call('getAchievementSummary', [g]); };

  // ── Vehicle browser (Nuts & Bolts) ───────────────────────────────────────────
  window.getVehicleCount  = function ()       { return call('getVehicleCount', []); };
  window.getVehicle       = function (i)      { return call('getVehicle', [i]); };
  window.reloadVehicles   = function ()       { return call('reloadVehicles', []); };

  // ── Google OAuth (system-browser loopback) ───────────────────────────────────
  // `GoogleSignIn` opens the system browser and starts the PKCE flow (fire-and-forget).
  // Poll `getGoogleSignInResult` every ~500 ms until status is "ok" or "error".
  // On "ok", use GoogleAuthProvider.credential(null, result.accessToken) + signInWithCredential.
  window.GoogleSignIn          = function ()  { return call('GoogleSignIn', []); };
  window.getGoogleSignInResult = function ()  { return call('getGoogleSignInResult', []); };

  // ── Offline mode ─────────────────────────────────────────────────────────────
  // `isOfflineMode` reflects the effective mode for this launch; `setOfflineMode`
  // persists an explicit, sticky choice and navigates immediately (no relaunch).
  window.isOfflineMode    = function ()       { return call('isOfflineMode', []); };
  window.setOfflineMode   = function (offline){ return call('setOfflineMode', [offline]); };
  // Cached connectivity status, refreshed by a background probe every ~20s —
  // cheap to poll (unlike a real probe, which can take seconds to time out),
  // so the UI can grey out "switch to online mode" while unreachable.
  window.isGoopieReachable = function ()      { return call('isGoopieReachable', []); };

  // ── Game-data disk cache (offline fallback) ──────────────────────────────────
  // Shape: `{ lastUpdated: <ISO-8601 string>, games: Game[] }`.
  window.getCachedGamesData = function ()     { return call('getCachedGamesData', []); };
  window.setCachedGamesData = function (data) { return call('setCachedGamesData', [data]); };

  // ── Misc ─────────────────────────────────────────────────────────────────────
  window.testFunction     = function (s)      { return call('testFunction', [s]); };

  console.log('[GoopieLauncher] bridge shim loaded (goopiebridge custom scheme)');
})();
