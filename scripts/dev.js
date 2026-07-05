// Runs `npm run dev:web` and `npm run dev:tauri` side by side, restarting
// either one if it crashes, until this script itself is stopped (Ctrl+C).
const { spawn } = require("child_process");

let stopping = false;
const children = new Set();

function run(name, npmScript) {
  const proc = spawn(`npm run ${npmScript}`, {
    stdio: "inherit",
    shell: true,
  });
  children.add(proc);

  proc.on("exit", (code, signal) => {
    children.delete(proc);
    if (stopping) return;
    console.log(`[${name}] exited (code=${code}, signal=${signal}), restarting...`);
    run(name, npmScript);
  });
}

function shutdown() {
  if (stopping) return;
  stopping = true;
  for (const proc of children) {
    proc.kill();
  }
}

process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);

run("web", "dev:web");
run("tauri", "dev:tauri");
