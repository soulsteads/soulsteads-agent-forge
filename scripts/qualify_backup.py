"""Stopped native Rust Forge backup/restore; fresh paths required."""
from pathlib import Path
import os,subprocess,json,hashlib,shutil
state=Path(os.environ['FORGE_TEST_STATE']).resolve();state.mkdir(parents=True,exist_ok=False)
binary=Path(os.environ['FORGE_BINARY']).resolve(strict=True);home=Path(os.environ['FORGE_TEST_HOME']).resolve();backup=state/'backup';restored=state/'restored'
assert binary.read_bytes()[:4]==b'\x7fELF'
subprocess.run([str(binary),'--home',str(home),'backup',str(backup)],check=True,timeout=60)
env={**os.environ,'FORGE_DATABASE_URL':os.environ['FORGE_RESTORE_DATABASE_URL']}
tampered=state/'tampered';shutil.copytree(backup,tampered)
config=tampered/'home/config.json';config.write_bytes(config.read_bytes()+b'\n')
rejected=subprocess.run([str(binary),'--home',str(state/'tampered-restore'),'restore',str(tampered)],env=env,capture_output=True,text=True,timeout=30)
assert rejected.returncode!=0,'tampered snapshot accepted'
(state/'tamper.stderr').write_text(rejected.stderr)
assert not (state/'tampered-restore').exists(),'tampered backup created a runtime home'
subprocess.run([str(binary),'--home',str(restored),'restore',str(backup)],env=env,check=True,timeout=60)
wrong=subprocess.run([str(binary),'--home',str(restored),'serve','--port','19102'],env=os.environ.copy(),capture_output=True,text=True,timeout=15)
assert wrong.returncode!=0,'restored home rebound to original database'
(state/'wrong-target.stderr').write_text(wrong.stderr)
assert any(word in (wrong.stdout+wrong.stderr).lower() for word in ['identity','seal','binding','bound','database']), 'wrong-target denial lacked diagnostic'
(state/'receipt.json').write_text(json.dumps({'backup_complete':True,'restore_complete':True,'source_home':str(home),'restored_home':str(restored),'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'runtime':'Rust ELF only','tampered_archive_denied':True,'restored_wrong_database_refused':True,'readback':'Pending service start and API/Codex proof against restored nativeDBOS state'},indent=2)+'\n')
print('Native stopped backup and restore completed; restored readback still required')
