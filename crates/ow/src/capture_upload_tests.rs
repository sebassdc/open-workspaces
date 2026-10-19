use super::*;
use std::{cell::RefCell, io::Cursor};
#[derive(Default)]
struct Memory {
    files: RefCell<BTreeMap<String, Vec<u8>>>,
    fail_manifest: RefCell<bool>,
}
impl Objects for Memory {
    fn put_once(&self, key: &str, source: &mut dyn Read) -> Result<()> {
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes)?;
        ensure!(
            !(*self.fail_manifest.borrow() && key.ends_with("manifest.json")),
            "injected manifest failure"
        );
        if let Some(old) = self.files.borrow().get(key) {
            ensure!(old == &bytes, "write once conflict");
        }
        self.files.borrow_mut().insert(key.into(), bytes);
        Ok(())
    }
    fn get(&self, key: &str) -> Result<Box<dyn Read>> {
        Ok(Box::new(Cursor::new(
            self.files
                .borrow()
                .get(key)
                .context("missing object")?
                .clone(),
        )))
    }
}
#[test]
fn ram_and_disk_publication_is_manifest_last_retryable_and_keeps_source() {
    for ram in [false, true] {
        let root = std::env::temp_dir().join(format!("ow-publish-{}", store::token().unwrap()));
        let path = root.join("snapshots/sealed");
        fs::create_dir_all(&path).unwrap();
        let mut hashes = serde_json::Map::new();
        for file in if ram {
            vec!["disk.ext4", "state", "memory"]
        } else {
            vec!["disk.ext4"]
        } {
            let bytes = if file == "disk.ext4" {
                b"disk".as_slice()
            } else {
                b"ram".as_slice()
            };
            fs::write(path.join(file), bytes).unwrap();
            hashes.insert(file.into(), json!(store::sha256(bytes)));
        }
        let native = json!({"workspace":"physical","name":"sealed","managed":true,"memory_mib":256,"vcpu_count":1,"captured_at":123,"files":hashes});
        fs::write(
            path.join("manifest.json"),
            serde_json::to_vec(&native).unwrap(),
        )
        .unwrap();
        let a = Artifact {
            id: "artifact".into(),
            machine: "physical".into(),
            store_url: "s3://bucket/root".into(),
            reference: Reference {
                attempt: "a".repeat(32),
                digest: String::new(),
            },
            snapshot: "sealed".into(),
            memory_mib: 256,
            cpus: 1,
            disk_bytes: 4,
        };
        let objects = Memory::default();
        *objects.fail_manifest.borrow_mut() = true;
        assert!(publish(&objects, &root, &a).is_err());
        assert!(!receipt(&root, &a).exists());
        assert_eq!(fs::read(path.join("disk.ext4")).unwrap(), b"disk");
        *objects.fail_manifest.borrow_mut() = false;
        let first = publish(&objects, &root, &a).unwrap();
        assert_eq!(publish(&objects, &root, &a).unwrap(), first);
        let pinned = Artifact {
            reference: serde_json::from_value(first["reference"].clone()).unwrap(),
            ..a.clone()
        };
        assert!(cache(&root, &pinned).unwrap()["present"] == true);
        let destination = root.join("other");
        fs::create_dir(&destination).unwrap();
        assert!(
            crate::transfers::pull(&objects, &destination, &pinned, &"c".repeat(32)).unwrap()["present"]
                == true
        );
        assert_eq!(
            fs::read(destination.join("snapshots/sealed/disk.ext4")).unwrap(),
            b"disk"
        );
        let progress: Value = serde_json::from_slice(
            &fs::read(root.join("transfer-progress").join(&a.reference.attempt)).unwrap(),
        )
        .unwrap();
        assert_eq!(progress["bytes_completed"], progress["bytes_total"]);
        fs::remove_dir_all(root).unwrap();
    }
}
