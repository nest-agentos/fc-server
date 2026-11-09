#!/usr/bin/env python3
"""Test source updates without root, systemd or a VM."""
import importlib.util
import json
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('service_update',Path(__file__).with_name('update-service.py'))
update=importlib.util.module_from_spec(spec);spec.loader.exec_module(update)


class Updates(unittest.TestCase):
    def setUp(self):
        self.temporary=tempfile.TemporaryDirectory();self.addCleanup(self.temporary.cleanup)
        self.root=Path(self.temporary.name);self.manifest=self.root/'service.toml';self.key=self.root/'identity'
        self.key.write_text('fixture-identity');(self.root/'config.toml').write_text('value="first"')
        self.manifest.write_text('id="test"\nprofile="test"\ndirectories=["prj"]\nvolumes=[]\ncommand=["/bin/true"]\nworking_directory="/home/agent"\nready_port=8080\n[environment]\nTOKEN="fixture-secret"\n[[files]]\nsource="config.toml"\ntarget="config.toml"\n')
        self.status={'service':'test','state':'ready','rootDisk':{'retained':True},'machine':{'profile':'test','directories':['prj'],'volumes':[]}}

    def test_snapshot_replaces_only_declared_files_without_editing_source_or_mixing_identity(self):
        stage=self.root/'stage';stage.mkdir();before=self.manifest.read_bytes()
        definition,manifest,key=update.snapshot(self.manifest,self.key,stage)
        self.assertTrue(definition['files'][0]['overwrite']);self.assertEqual(self.manifest.read_bytes(),before)
        self.assertEqual(tomllib.loads(manifest.read_text()),definition)
        self.assertEqual(key.read_text(),'fixture-identity')
        (self.root/'config.toml').write_text('changed')
        self.assertEqual((stage/definition['files'][0]['source']).read_text(),'value="first"')

    def test_numeric_environment_values_fail_before_any_service_command(self):
        self.manifest.write_text(self.manifest.read_text().replace('TOKEN="fixture-secret"','TOKEN=160000'))
        with patch.object(update,'invoke') as invoke:
            with self.assertRaisesRegex(update.UpdateError,'ENVIRONMENT_VALUE_MUST_BE_STRING'):
                update.update('server','vm',self.manifest,self.key)
            invoke.assert_not_called()

    def test_invalid_or_temporary_roots_are_rejected_before_stopping(self):
        self.status['rootDisk']['retained']=False
        with patch.object(update,'invoke',return_value=self.status) as invoke:
            with self.assertRaisesRegex(update.UpdateError,'PERSISTENT_ROOT'):
                update.update('server','vm',self.manifest,self.key)
            self.assertEqual(invoke.call_count,1)

    def test_update_preserves_enabled_state_and_never_destroys_root(self):
        calls=[]
        def invoke(server,*args):
            calls.append(args)
            if args[:2]==('service','install'):
                content=tomllib.loads(Path(args[2]).read_text());self.assertTrue(content['files'][0]['overwrite'])
            return self.status
        with patch.object(update,'invoke',side_effect=invoke),patch.object(update,'enabled',return_value=True):
            update.update('server','vm',self.manifest,self.key)
        self.assertEqual([args[1] for args in calls],['status','stop','install','up','status'])
        self.assertEqual(calls[-2],('service','up','vm','test','--enable'))
        self.assertFalse(any('destroy' in args for args in calls))

    def test_failed_install_does_not_start_or_retry(self):
        calls=[]
        def invoke(server,*args):
            calls.append(args)
            if args[1]=='install':raise update.UpdateError('SERVICE_UPDATE_REJECTED')
            return self.status
        with patch.object(update,'invoke',side_effect=invoke),patch.object(update,'enabled',return_value=False):
            with self.assertRaisesRegex(update.UpdateError,'REJECTED'):
                update.update('server','vm',self.manifest,self.key)
        self.assertEqual([args[1] for args in calls],['status','stop','install'])

    def test_mismatched_service_storage_and_symlink_sources_are_rejected(self):
        for replacement in [{'service':'other'},{'machine':{'profile':'test','directories':[],'volumes':[]}}]:
            with self.assertRaises(update.UpdateError):update.preflight('vm',tomllib.loads(self.manifest.read_text()),{**self.status,**replacement})
        (self.root/'config.toml').unlink();(self.root/'config.toml').symlink_to(self.key)
        with self.assertRaisesRegex(update.UpdateError,'INVALID_FILE_SOURCE'):
            update.snapshot(self.manifest,self.key,self.root/'stage')

if __name__=='__main__':unittest.main()
