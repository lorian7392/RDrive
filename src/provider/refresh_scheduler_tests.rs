use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn cached_catalog_remains_available_when_browse_hint_storage_is_unwritable() {
    let (_temporary,store,mut domain)=coverage_tests::setup();domain.smart=true;add_domain(&store,domain.clone()).unwrap();
    let backend=coverage_tests::Tree{entries:std::sync::Mutex::new(vec![coverage_tests::entry("cached.txt",false)]),unsafe_listing:std::sync::atomic::AtomicBool::new(false)};
    let mut journal=checked_journal(&store.home,&store.config,&domain).unwrap();
    refresh_directory(&store,&backend,&domain,&mut journal,"root").await.unwrap();
    journal.directory_scans.insert("root".into(),0);save_journal(&store,&domain.id,&journal).unwrap();
    let home=store.home.clone();drop(store);
    let hints=demand_directory(&home,&domain.id);fs::create_dir(&hints).unwrap();fs::set_permissions(&hints,fs::Permissions::from_mode(0o755)).unwrap();
    let stage=home.join("stage");fs::create_dir(&stage).unwrap();let fd=open_staging(&stage).unwrap();
    let request=serde_json::from_value(json!({"v":1,"op":"list","domain":domain.id,"parent_id":"root"})).unwrap();
    let cached=process(&home,&stage,&fd,request,Default::default()).await.unwrap();
    assert_eq!(cached["items"][0]["name"],"cached.txt");
    let refresh=serde_json::from_value(json!({"v":1,"op":"refresh_index","domain":domain.id,"id":"root"})).unwrap();
    assert!(process(&home,&stage,&fd,refresh,Default::default()).await.is_err(),"explicit refresh must report failed demand persistence");
}
