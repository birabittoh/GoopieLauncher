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

  // ── Config / paths ───────────────────────────────────────────────────────────
  window.GetGamesPath    = function ()    { return call('GetGamesPath', []); };
  window.SetGamesPath    = function ()    { return call('SetGamesPath', []); };
  window.GetLanguage     = function ()    { return call('GetLanguage', []); };
  window.SetLanguage     = function (l)   { return call('SetLanguage', [l]); };

  // ── Game state ───────────────────────────────────────────────────────────────
  window.isIsoInstalled      = function (g)            { return call('isIsoInstalled', [g]); };
  window.isExeUpdated        = function (g, b)         { return call('isExeUpdated', [g, b]); };
  // `build` (below) identifies an installed build by its on-disk key — i.e.
  // the `name` field returned by getInstalledBuilds(g), which equals the
  // sanitised release tag the build was installed under.
  window.getInstalledVersion = function (g, b)         { return call('getInstalledVersion', [g, b]); };
  window.getInstalledBuilds  = function (g)            { return call('getInstalledBuilds', [g]); };

  // ── Long-running ops (fire-and-forget; poll for progress) ────────────────────
  window.Install         = function (g)                { return call('Install', [g]); };
  window.Uninstall       = function (g, b)             { return call('Uninstall', [g, b]); };
  window.UninstallAll    = function (g)                { return call('UninstallAll', [g]); };
  window.Update          = function (g, u, a, v, p)    { return call('Update', [g, u, a, v, p]); };
  window.NeedsUpdate     = function (g, b, u, a)       { return call('NeedsUpdate', [g, b, u, a]); };
  window.Play            = function (g, b, c, e, r)    { return call('Play', [g, b, c, e, r]); };
  window.InstallPackage  = function (g, b, p, z, h, e) { return call('InstallPackage', [g, b, p, z, h, e]); };
  window.IsPackageInstalled = function (g, b, z)       { return call('IsPackageInstalled', [g, b, z]); };

  // ── Progress polling ─────────────────────────────────────────────────────────
  window.isExtracting        = function ()    { return call('isExtracting', []); };
  window.isUpdating          = function ()    { return call('isUpdating', []); };
  window.getDownloadProgress = function ()    { return call('getDownloadProgress', []); };
  window.getDownloadString   = function ()    { return call('getDownloadString', []); };
  window.getExtractProgress  = function ()    { return call('getExtractProgress', []); };

  // ── Folder operations ────────────────────────────────────────────────────────
  window.OpenGamesFolder  = function ()       { return call('OpenGamesFolder', []); };
  window.openSaveFolder   = function (g)      { return call('openSaveFolder', [g]); };
  window.OpenExternalLink = function (url)    { return call('OpenExternalLink', [url]); };

  // ── Save management ──────────────────────────────────────────────────────────
  window.getSaveSlots         = function (g)       { return call('getSaveSlots', [g]); };
  window.getSaveSlotCount     = function (g)       { return call('getSaveSlotCount', [g]); };
  window.getActiveSave        = function (g)       { return call('getActiveSave', [g]); };
  window.backupSave           = function (g, s)    { return call('backupSave', [g, s]); };
  window.restoreSave          = function (g, s)    { return call('restoreSave', [g, s]); };
  window.deleteSave           = function (g, s)    { return call('deleteSave', [g, s]); };
  window.renameSave           = function (g, o, n) { return call('renameSave', [g, o, n]); };
  window.deleteCurrentSave    = function (g)       { return call('deleteCurrentSave', [g]); };

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

  // ── Misc ─────────────────────────────────────────────────────────────────────
  window.testFunction     = function (s)      { return call('testFunction', [s]); };

  console.log('[GoopieLauncher] bridge shim loaded (goopiebridge custom scheme)');
})();
