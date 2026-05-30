// Goopie Launcher – synchronous native bridge shim
// Injected as an initialization_script before any page JavaScript runs.
// __BRIDGE_PORT__ and __BRIDGE_TOKEN__ are substituted at runtime.
(function () {
  'use strict';

  var BASE = 'http://127.0.0.1:__BRIDGE_PORT__/bridge/';
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
  window.isExeUpdated        = function (g)            { return call('isExeUpdated', [g]); };
  window.getInstalledVersion = function (g)            { return call('getInstalledVersion', [g]); };

  // ── Long-running ops (fire-and-forget; poll for progress) ────────────────────
  window.Install         = function (g)                { return call('Install', [g]); };
  window.Uninstall       = function (g)                { return call('Uninstall', [g]); };
  window.Update          = function (g, u, a, v, p)    { return call('Update', [g, u, a, v, p]); };
  window.NeedsUpdate     = function (g, u, a)          { return call('NeedsUpdate', [g, u, a]); };
  window.Play            = function (g, c, e, r)       { return call('Play', [g, c, e, r]); };
  window.InstallPackage  = function (g, p, z, h, e)    { return call('InstallPackage', [g, p, z, h, e]); };
  window.IsPackageInstalled = function (g, z)          { return call('IsPackageInstalled', [g, z]); };

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

  // ── Misc ─────────────────────────────────────────────────────────────────────
  window.testFunction     = function (s)      { return call('testFunction', [s]); };

  console.log('[GoopieLauncher] bridge shim loaded, port __BRIDGE_PORT__');
})();
