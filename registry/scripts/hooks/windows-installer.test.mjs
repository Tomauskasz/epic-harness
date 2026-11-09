import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

test('Windows installer reports locked replacement and hash errors without claiming success', { skip: process.platform !== 'win32' }, () => {
  const source = fs.readFileSync(new URL('../../../install.ps1', import.meta.url), 'utf8').replace(/param\([\s\S]*?\r?\n\)/, '');
  for (const scenario of ['fresh', 'repeat', 'locked', 'bad-hash']) {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'epic-installer-'));
    try {
      const script = path.join(root, 'fixture.ps1');
      const prefix = String.raw`
$fixtureRoot = $env:EPIC_INSTALL_FIXTURE
$scenario = $env:EPIC_INSTALL_SCENARIO
$InstallDir = Join-Path $fixtureRoot install
$payload = Join-Path $fixtureRoot payload
New-Item -ItemType Directory -Path $payload,$InstallDir -Force | Out-Null
Set-Content -LiteralPath (Join-Path $payload epic.exe) -Value 'SYNTHETIC NEW BINARY' -NoNewline
Set-Content -LiteralPath (Join-Path $payload epic-harness.exe) -Value 'SYNTHETIC NEW BINARY' -NoNewline
$fixtureZip = Join-Path $fixtureRoot fixture.zip
Compress-Archive -Path (Join-Path $payload '*') -DestinationPath $fixtureZip
$fixtureHash = (Get-FileHash -LiteralPath $fixtureZip -Algorithm SHA256).Hash.ToLower()
function Invoke-RestMethod { return @{ tag_name='v9.9.9' } }
function Invoke-WebRequest {
 param([string]$Uri,[string]$OutFile,[switch]$UseBasicParsing)
 if ($Uri.EndsWith('.sha256')) {
   if ($scenario -eq 'bad-hash') { Set-Content -LiteralPath $OutFile -Value ('0'*64 + ' fixture.zip') }
   else { Set-Content -LiteralPath $OutFile -Value ($fixtureHash + ' fixture.zip') }
 } else { Copy-Item -LiteralPath $fixtureZip -Destination $OutFile }
}
if ($scenario -ne 'fresh') { Set-Content -LiteralPath (Join-Path $InstallDir epic.exe) -Value 'SYNTHETIC OLD BINARY' -NoNewline }
if ($scenario -eq 'locked') { $ownedLock = [IO.File]::Open((Join-Path $InstallDir epic.exe),[IO.FileMode]::Open,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None) }
`;
      fs.writeFileSync(script, prefix + '\n' + source);
      const home = path.join(root, 'home'), data = path.join(root, 'data');
      fs.mkdirSync(path.join(home, '.config/epic-harness'), { recursive: true });fs.mkdirSync(data);
      fs.writeFileSync(path.join(home, '.config/epic-harness/telemetry-consent'), 'off');
      fs.writeFileSync(path.join(data, 'config.toml'), '[dashboard]\nport=0\nauto_open=false\n');
      const env = { ...process.env, HOME: home, USERPROFILE: home, HARNESS_DIR: data, HARNESS_ROOT: root, TEMP: root, TMP: root, EPIC_INSTALL_FIXTURE: root, EPIC_INSTALL_SCENARIO: scenario };
      for (const k of Object.keys(env)) if (k.toLowerCase() === 'path') delete env[k];
      env.PATH = path.join(root, 'install') + path.delimiter + path.join(process.env.SystemRoot, 'System32');
      const result = spawnSync(path.join(process.env.SystemRoot, 'System32/WindowsPowerShell/v1.0/powershell.exe'), ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', script], { env, cwd: root, windowsHide: true, encoding: 'utf8', timeout: 20000 });
      const contents = fs.existsSync(path.join(root, 'install/epic.exe')) ? fs.readFileSync(path.join(root, 'install/epic.exe'), 'utf8') : null;
      if (scenario === 'locked' || scenario === 'bad-hash') {
        assert.notEqual(result.status, 0, `${scenario}: ${result.stdout}\n${result.stderr}`);
        assert.doesNotMatch(result.stdout, /Installed:/, 'a failed copy/hash check must never claim installation');
        assert.equal(contents, 'SYNTHETIC OLD BINARY');
      } else {
        assert.equal(result.status, 0, `${scenario}: ${result.stderr}`);
        assert.equal(contents, 'SYNTHETIC NEW BINARY');
        assert.match(result.stdout, /Installed:/);
      }
    } finally {
      assert.ok(path.resolve(root).startsWith(path.resolve(os.tmpdir()) + path.sep));
      fs.rmSync(root, { recursive: true, force: true });
    }
  }
});
