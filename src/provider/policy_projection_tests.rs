//! Differential projection tests retain the former implementation as an oracle.
use super::*;

fn reference_policies(store:&Store,d:&Domain,j:&Journal,records:&mut BTreeMap<String,Record>)->Result<()> {
    let paths=records.iter().map(|(id,r)|(r.path.clone(),id.clone())).collect::<BTreeMap<_,_>>();
    for record in records.values_mut(){record.item.name=record.path.rsplit('/').next().unwrap().into();let parent=record.path.rsplit_once('/').map(|(p,_)|p).unwrap_or("");record.item.parent_id=if parent.is_empty(){"root".into()}else{paths.get(parent).cloned().context("provider canonical parent missing")?};}
    let pins:BTreeMap<_,_>=records.keys().map(|id|(id.clone(),explicit_pin(j,id,records))).collect();
    let mutations=if d.smart {crate::smart::read_only(&store.home,&d.id,&journal_binding(&store.config,d)?)?}else{vec![]};
    let native=read_native_cached(&store.home,&store.config,d)?;
    let deleted=mutations.iter().filter(|m|m.state.active()&&m.request["op"]=="delete").filter_map(|m|m.request["id"].as_str()).filter_map(|id|records.get(id).map(|r|r.path.clone())).collect::<Vec<_>>();
    for (id,record) in records {
        record.item.pinned=pins.get(id).copied().unwrap_or(false);record.item.smart=d.smart;record.item.native_materialized=native.get(id)==Some(&record.item.content_version);
        let pending=mutations.iter().rev().find(|m|m.state.active()&&(m.request["id"].as_str()==Some(id)||j.smart_replays.get(&m.id).and_then(|r|r.id.as_ref())==Some(id)));
        let hidden=deleted.iter().any(|path|record.path==*path||record.path.starts_with(&format!("{path}/")));
        if let Some(mutation)=pending.filter(|m|m.request["op"]=="modify"){
            if let Some(name)=mutation.request["name"].as_str(){if !name.is_empty()&&!name.contains('/'){record.item.name=name.into();}}
            if let Some(parent)=mutation.request["parent_id"].as_str(){record.item.parent_id=parent.into();}
        }
        record.item.has_unsynced_changes=pending.is_some()||hidden;
        let (state,message)=if hidden {("queued_delete",pending.and_then(|m|m.error.clone()))}else if let Some(m)=pending {use crate::smart::MutationState::*;let blocked=m.error.as_deref().and_then(crate::filtering::exclusion_path).is_some();let state=match m.state {Conflict=>"conflict",Running=>"syncing",_ if blocked=>"blocked",Failed=>"error",_=>"queued"};(state,m.error.clone())}
            else if let Some(error)=j.native_errors.get(id).filter(|e|e.version==record.item.content_version){("error",Some(error.message.clone()))}else if record.item.pinned {("pinned",None)}else if native.get(id)==Some(&record.item.content_version){("cached",None)}else {("online",None)};
        record.item.sync_state=Some(state.into());record.item.sync_message=message;
        let base=metadata_version(&record.path,&record.item.tag_data_hex,&record.item.extended_attributes,record.item.pinned);
        record.item.metadata_version=format!("{base}:{}",&state::key(&serde_json::to_string(&(&record.item.sync_state,&record.item.sync_message,record.item.has_unsynced_changes,record.item.native_materialized))?)[..32]);
    }Ok(())
}

fn record(id:&str,path:&str,directory:bool)->Record {
    let mut item=root_item(&Domain{id:"test".into(),profile:"p".into(),remote:String::new(),display_name:"Test".into(),repository:false,local_root:None,smart:true});
    item.id=id.into();item.kind=if directory{"directory"}else{"file"}.into();item.name="old projected name".into();item.parent_id="old projected parent".into();item.content_version=format!("content:{id}");item.tag_data_hex=Some("cafe".into());item.extended_attributes.insert("user.rdrive-test#S".into(),"value".into());
    Record{path:path.into(),item,entry:None,strong:None,client_id:None,metadata_known:false,metadata_checked:0}
}
fn mutation(binding:&str,domain:&Domain,sequence:u64,target:&str,op:&str,state:crate::smart::MutationState)->crate::smart::Mutation {
    let request=json!({"v":1,"op":op,"domain":domain.id,"id":target,"request_id":format!("request:{sequence}"),"base_content_version":"base-content","base_metadata_version":"base-metadata","name":format!("renamed-{sequence}"),"parent_id":"target-parent"});
    let id=state::key(&format!("{binding}\0request:request:{sequence}"));let fingerprint=state::key(&serde_json::to_string(&(request.clone(),Option::<Value>::None)).unwrap());
    crate::smart::Mutation{id,sequence,request,fingerprint,snapshot:None,payload_discarded:false,state,attempts:1,created:state::now(),updated:state::now(),error:None,response:(state==crate::smart::MutationState::Done).then(||json!({"ok":true})),superseded_by:None,resolution:None}
}
fn persist_mutations(store:&Store,domain:&Domain,mutations:&[crate::smart::Mutation]) {
    let binding=journal_binding(&store.config,domain).unwrap();drop(crate::smart::Queue::open(store,&domain.id,&binding).unwrap());
    let entries: BTreeMap<_,_>=mutations.iter().map(|m|(m.id.clone(),m)).collect();
    state::atomic_json(&store.home.join("smart").join(state::key(&domain.id)).join("queue.json"),&json!({"version":1,"domain":domain.id,"binding":binding,"next_sequence":mutations.iter().map(|m|m.sequence).max().unwrap_or(0)+1,"mutations":entries})).unwrap();
    assert_eq!(crate::smart::read_only(&store.home,&domain.id,&binding).unwrap().len(),mutations.len());
}
fn assert_equivalent(store:&Store,domain:&Domain,journal:&Journal,records:&BTreeMap<String,Record>)->BTreeMap<String,Record> {
    let mut expected=records.clone();reference_policies(store,domain,journal,&mut expected).unwrap();
    let mut actual=records.clone();apply_policies(store,domain,journal,&mut actual).unwrap();
    assert_eq!(serde_json::to_value(&actual).unwrap(),serde_json::to_value(&expected).unwrap());actual
}

#[test]
fn indexed_mutation_projection_matches_reference_for_aliases_states_and_deletion_ancestors() {
    use crate::smart::MutationState::*;
    let(_tmp,store,mut domain)=coverage_tests::setup();domain.smart=true;let binding=journal_binding(&store.config,&domain).unwrap();
    let records=BTreeMap::from([("dir".into(),record("dir","docs",true)),("nested".into(),record("nested","docs/sub",true)),("child".into(),record("child","docs/sub/file",false)),("prefix".into(),record("prefix","docs2",true)),("other".into(),record("other","docs2/other",false)),("mapped".into(),record("mapped","mapped",false)),("native".into(),record("native","native",false)),("attention".into(),record("attention","attention",false)),("stateful".into(),record("stateful","stateful",false)),("created".into(),record("created","created",true))]);
    for state in [Pending,Running,Interrupted,Conflict,Failed,Done,Cancelled,Superseded] {
        let mut j=Journal::default();j.items=records.clone();j.pins.insert("dir".into());j.native_errors.insert("attention".into(),NativeAttention{version:"content:attention".into(),message:"retained native error".into()});
        let mut mutations=vec![mutation(&binding,&domain,1,"dir","delete",Pending),mutation(&binding,&domain,2,"stateful","modify",state),mutation(&binding,&domain,3,"absent","modify",Pending),mutation(&binding,&domain,4,"mapped","modify",Pending),mutation(&binding,&domain,5,"other","modify",Failed),mutation(&binding,&domain,6,"absent-delete","delete",Pending),mutation(&binding,&domain,7,"child","modify",Pending)];
        let mut queue=crate::smart::Queue::open(&store,&domain.id,&binding).unwrap();
        let mut creation=queue.enqueue(json!({"v":1,"op":"create","domain":domain.id,"client_item_id":"local-created-directory","parent_id":"root","name":"created","kind":"directory"}),None).unwrap();drop(queue);creation.sequence=8;
        j.smart_replays.insert(creation.id.clone(),serde_json::from_value(json!({"v":1,"op":"create","domain":domain.id,"id":"created"})).unwrap());mutations.push(creation);
        if state==Superseded {mutations[1].superseded_by=Some(mutations[3].id.clone());}
        mutations[4].error=Some("excluded_by_filter: synchronization filters exclude docs2/other; local mutation retained until the rules allow it".into());
        let alias:Request=serde_json::from_value(json!({"v":1,"op":"modify","domain":domain.id,"id":"mapped"})).unwrap();j.smart_replays.insert(mutations[2].id.clone(),alias);
        state::atomic_json(&native_cached_path(&store.home,&domain.id),&NativeCached{binding:binding.clone(),versions:BTreeMap::from([("native".into(),"content:native".into())])}).unwrap();
        persist_mutations(&store,&domain,&mutations);let projected=assert_equivalent(&store,&domain,&j,&records);
        assert_eq!(projected["child"].item.sync_state.as_deref(),Some("queued_delete"));assert!(projected["child"].item.pinned);
        assert_eq!(projected["stateful"].item.sync_state.as_deref(),Some(match state {Pending|Interrupted=>"queued",Running=>"syncing",Conflict=>"conflict",Failed=>"error",Done|Cancelled|Superseded=>"online"}));
        assert_eq!(projected["mapped"].item.name,"renamed-4","newer direct identity takes precedence over older replay alias");
        assert_eq!(projected["native"].item.sync_state.as_deref(),Some("cached"));assert_eq!(projected["attention"].item.sync_state.as_deref(),Some("error"));assert_eq!(projected["other"].item.sync_state.as_deref(),Some("blocked"));assert!(!projected["prefix"].item.has_unsynced_changes,"deletion of docs must not hide docs2");
        let alias:Request=serde_json::from_value(json!({"v":1,"op":"modify","domain":domain.id,"id":"mapped"})).unwrap();j.smart_replays.insert(mutations[4].id.clone(),alias);
        let projected=assert_equivalent(&store,&domain,&j,&records);assert_eq!(projected["mapped"].item.name,"renamed-5","newer replay alias takes precedence over older direct identity");assert_eq!(projected["mapped"].item.parent_id,"target-parent");
        domain.smart=false;assert_equivalent(&store,&domain,&j,&records);domain.smart=true;
    }
}

#[test]
#[ignore = "Explicit release-mode projection benchmark; reports timings without a CI threshold"]
fn large_projection_benchmark_preserves_the_complete_reference_output() {
    use std::time::Instant;use crate::smart::MutationState;
    let(_tmp,store,mut domain)=coverage_tests::setup();domain.smart=true;let binding=journal_binding(&store.config,&domain).unwrap();let mut records=BTreeMap::new();
    for i in 0..1000 {let id=format!("dir-{i:04}");records.insert(id.clone(),record(&id,&format!("dir{i:04}"),true));}
    for i in 0..107675 {let id=format!("file-{i:06}");records.insert(id.clone(),record(&id,&format!("dir{:04}/file{i:06}",i%1000),false));}
    let mutations=(1..=529).map(|i|{let directory=i%10==0;let target=if directory{format!("dir-{:04}",i%1000)}else{format!("file-{:06}",i*100)};mutation(&binding,&domain,i,&target,if directory{"delete"}else{"modify"},if i%3==0{MutationState::Interrupted}else{MutationState::Pending})}).collect::<Vec<_>>();persist_mutations(&store,&domain,&mutations);
    let journal=Journal::default();let mut expected=records.clone();let start=Instant::now();reference_policies(&store,&domain,&journal,&mut expected).unwrap();let before=start.elapsed();
    let mut actual=records;let start=Instant::now();apply_policies(&store,&domain,&journal,&mut actual).unwrap();let after=start.elapsed();
    for (id,record) in &actual {assert_eq!(serde_json::to_value(record).unwrap(),serde_json::to_value(&expected[id]).unwrap(),"{id}");}
    println!("POLICY_PROJECTION_BENCHMARK {}",json!({"items":actual.len(),"mutations":mutations.len(),"reference_ms":before.as_secs_f64()*1000.0,"optimized_ms":after.as_secs_f64()*1000.0,"speedup":before.as_secs_f64()/after.as_secs_f64(),"outputs_identical":true}));
    let record_json_bytes=actual.values().map(|record|serde_json::to_vec(record).unwrap().len()).sum::<usize>();
    let mut persisted=checked_journal(&store.home,&store.config,&domain).unwrap();persisted.items=actual;persisted.filter_fingerprint=Some(state::key(&serde_json::to_string(&store.config.filters).unwrap()));
    let start=Instant::now();save_journal(&store,&domain.id,&persisted).unwrap();let initial_save=start.elapsed();
    persisted.directory_turn+=1;
    let start=Instant::now();save_journal(&store,&domain.id,&persisted).unwrap();let control_only_save=start.elapsed();
    let start=Instant::now();let loaded=index::load_existing(&store.home,&domain.id,&binding).unwrap().unwrap();let read_existing=start.elapsed();assert_eq!(loaded.items.len(),108675);
    let start=Instant::now();let checked=checked_journal(&store.home,&store.config,&domain).unwrap();let read_checked=start.elapsed();assert_eq!(checked.items.len(),108675);
    let start=Instant::now();update_overlay(&store,&domain).unwrap();let overlay=start.elapsed();
    let final_journal=index::load_existing(&store.home,&domain.id,&binding).unwrap().unwrap();assert_eq!(final_journal.anchor,persisted.anchor,"unchanged projection must not emit namespace changes");assert_eq!(final_journal.items.len(),persisted.items.len());
    let start=Instant::now();reference_update_overlay(&store,&domain).unwrap();let reference_noop=start.elapsed();
    let before_noop=index::load_existing(&store.home,&domain.id,&binding).unwrap().unwrap();
    let start=Instant::now();update_overlay(&store,&domain).unwrap();let optimized_noop=start.elapsed();
    let after_noop=index::load_existing(&store.home,&domain.id,&binding).unwrap().unwrap();assert_eq!(serde_json::to_value(&before_noop).unwrap(),serde_json::to_value(&after_noop).unwrap());
    println!("OVERLAY_NOOP_BENCHMARK {}",json!({"synthetic":true,"items":persisted.items.len(),"mutations":mutations.len(),"normalization_already_durable":true,"reference_ms":reference_noop.as_secs_f64()*1000.0,"optimized_ms":optimized_noop.as_secs_f64()*1000.0,"speedup":reference_noop.as_secs_f64()/optimized_noop.as_secs_f64(),"full_journal_identical":true}));
    let database=store.home.join("provider").join(format!("{}.sqlite3",state::key(&domain.id)));
    println!("POLICY_PIPELINE_BENCHMARK {}",json!({"synthetic":true,"items":persisted.items.len(),"mutations":mutations.len(),"events":persisted.events.len(),"record_json_bytes":record_json_bytes,"average_record_json_bytes":record_json_bytes as f64/persisted.items.len() as f64,"database_bytes":database.metadata().unwrap().len(),"initial_save_ms":initial_save.as_secs_f64()*1000.0,"control_only_save_ms":control_only_save.as_secs_f64()*1000.0,"load_existing_ms":read_existing.as_secs_f64()*1000.0,"checked_journal_ms":read_checked.as_secs_f64()*1000.0,"update_overlay_ms":overlay.as_secs_f64()*1000.0}));
}

fn reference_next_metadata(store:&Store,d:&Domain,j:&Journal,demands:&[CatalogDemand])->Result<Option<String>>{
    let selector=crate::filtering::Selector::new(&store.config,&d.profile)?;
    let now=chrono::Utc::now().timestamp_millis();
    Ok(j.items.iter().filter(|(id,record)|record.item.kind=="file"&&selector.allowed(&record.path,false).unwrap_or(false)&&record.strong.is_some()&&j.metadata_attempts.get(*id).is_none_or(|attempt|attempt.next_try<=now)).filter_map(|(id,record)|{
        let request=demands.iter().find(|d|d.metadata&&d.id==*id).map(|d|d.requested).unwrap_or(0);
        let priority=if request>record.metadata_checked{0}else if record.metadata_known&&now.saturating_sub(record.metadata_checked)>=30_000{1}else{return None};
        Some(((priority,j.metadata_attempts.get(id).map_or(0,|attempt|attempt.last),record.metadata_checked),id.clone()))
    }).min_by_key(|(priority,_)|*priority).map(|(_,id)|id))
}

fn metadata_record(id:&str,path:&str)->Record {let mut r=record(id,path,false);r.strong=Some(coverage_tests::entry(path,false));r}
fn demand(id:&str,requested:i64,metadata:bool)->CatalogDemand {CatalogDemand{binding:"test-binding".into(),id:id.into(),path:"test-path".into(),metadata,requested,recursive:false,force:false,finder:false}}
fn assert_metadata_equivalent(store:&Store,domain:&Domain,j:&Journal,demands:&[CatalogDemand])->Option<String> {let expected=reference_next_metadata(store,domain,j,demands).unwrap();let actual=next_metadata(store,domain,j,demands).unwrap();assert_eq!(actual,expected);actual}

#[test]
fn indexed_metadata_demands_preserve_first_match_priorities_backoff_and_visibility() {
    let(_tmp,mut store,domain)=coverage_tests::setup();let now=chrono::Utc::now().timestamp_millis();let mut j=Journal::default();let mut item=metadata_record("same","same");item.metadata_checked=now-1000;j.items.insert("same".into(),item);
    let duplicates=vec![demand("same",now,false),demand("missing",now,true),demand("same",now-2000,true),demand("same",now,true)];
    assert_eq!(assert_metadata_equivalent(&store,&domain,&j,&duplicates),None,"first metadata demand wins even when a later duplicate is newer");
    let reverse=vec![demand("same",now,true),demand("same",now-2000,true)];assert_eq!(assert_metadata_equivalent(&store,&domain,&j,&reverse).as_deref(),Some("same"));
    j.items.get_mut("same").unwrap().metadata_known=true;j.items.get_mut("same").unwrap().metadata_checked=now-60000;
    assert_eq!(assert_metadata_equivalent(&store,&domain,&j,&[]).as_deref(),Some("same"),"known stale metadata refreshes without a demand");
    j.items.get_mut("same").unwrap().metadata_checked=now-1000;assert_eq!(assert_metadata_equivalent(&store,&domain,&j,&[]),None,"fresh known metadata observes its TTL");
    j.items.get_mut("same").unwrap().metadata_known=false;
    for id in ["demanded","cold","retry","hidden.tmp","unverified","directory"] {let mut r=metadata_record(id,id);r.metadata_checked=now-60000;r.metadata_known=true;if id=="unverified"{r.strong=None;}if id=="directory"{r.item.kind="directory".into();}j.items.insert(id.into(),r);}
    j.metadata_attempts.insert("retry".into(),ScanAttempt{last:0,failures:1,next_try:now+60000});
    let demands=vec![demand("retry",now,true),demand("hidden.tmp",now,true),demand("unverified",now,true),demand("directory",now,true),demand("demanded",now,true)];
    assert_eq!(assert_metadata_equivalent(&store,&domain,&j,&demands).as_deref(),Some("demanded"),"eligible demanded metadata outranks stale background metadata");
    j.metadata_attempts.insert("demanded".into(),ScanAttempt{last:now-1000,failures:0,next_try:0});j.items.get_mut("same").unwrap().metadata_checked=now-60000;j.items.get_mut("same").unwrap().metadata_known=true;
    assert_eq!(assert_metadata_equivalent(&store,&domain,&j,&[]).as_deref(),Some("cold"),"oldest attempt then stable item order breaks equal priorities");
    j.items.get_mut("cold").unwrap().metadata_checked=now-300000;assert_eq!(assert_metadata_equivalent(&store,&domain,&j,&[]).as_deref(),Some("cold"));
    store.config.filters.exclude.push("cold".into());assert_eq!(assert_metadata_equivalent(&store,&domain,&j,&[]).as_deref(),Some("same"));
    store.config.filters.exclude.push("[".into());assert!(next_metadata(&store,&domain,&j,&demands).is_err());assert!(reference_next_metadata(&store,&domain,&j,&demands).is_err());
}

#[test]
#[ignore = "Explicit release-mode metadata-demand benchmark; reports timings without a CI threshold"]
fn large_metadata_demand_benchmark_preserves_the_reference_selection() {
    use std::time::Instant;
    let(_tmp,store,domain)=coverage_tests::setup();let mut j=Journal::default();
    for i in 0..108675 {let id=format!("file-{i:06}");let mut r=metadata_record(&id,&format!("file{i:06}"));r.metadata_known=true;j.items.insert(id,r);}
    let now=chrono::Utc::now().timestamp_millis();let demands=(0..14590).map(|i|demand(&format!("file-{:06}",(i*7)%108675),now-1000,true)).collect::<Vec<_>>();
    let start=Instant::now();let expected=reference_next_metadata(&store,&domain,&j,&demands).unwrap();let before=start.elapsed();
    let start=Instant::now();let actual=next_metadata(&store,&domain,&j,&demands).unwrap();let after=start.elapsed();assert_eq!(actual,expected);
    println!("METADATA_DEMAND_BENCHMARK {}",json!({"synthetic":true,"items":j.items.len(),"metadata_demands":demands.len(),"reference_ms":before.as_secs_f64()*1000.0,"optimized_ms":after.as_secs_f64()*1000.0,"speedup":before.as_secs_f64()/after.as_secs_f64(),"selection_identical":true}));
}

fn reference_update_overlay(store:&Store,d:&Domain)->Result<()> {
    let mut j=checked_journal(&store.home,&store.config,d)?;let old=j.items.clone();let mut next=std::mem::take(&mut j.items);apply_policies(store,d,&j,&mut next)?;
    let filter=state::key(&serde_json::to_string(&store.config.filters)?);let changed=j.filter_fingerprint.as_ref()!=Some(&filter);
    for (id,r) in &next {if changed||old.get(id).is_none_or(|old|old.item!=r.item){push_event(&mut j,Some(r.item.clone()),None);}}
    j.filter_fingerprint=Some(filter);j.items=next;save_journal(store,&d.id,&j)
}

#[test]
fn unchanged_overlay_skips_writes_but_persists_status_filters_and_read_normalization() {
    let(_tmp,mut store,mut domain)=coverage_tests::setup();domain.smart=true;let binding=journal_binding(&store.config,&domain).unwrap();
    let mut j=checked_journal(&store.home,&store.config,&domain).unwrap();let mut file=record("file","file",false);file.metadata_known=true;j.items.insert("file".into(),file);
    j.filter_fingerprint=Some(state::key(&serde_json::to_string(&store.config.filters).unwrap()));j.directory_turn=42;j.completed_demands.insert("retained-demand".into(),100);
    j.requests.insert("retained-receipt".into(),serde_json::from_value(json!({"fingerprint":"retained","response":{"kept":true}})).unwrap());
    let mut projected=j.items.clone();apply_policies(&store,&domain,&j,&mut projected).unwrap();j.items=projected;save_journal(&store,&domain.id,&j).unwrap();
    let database=store.home.join("provider").join(format!("{}.sqlite3",state::key(&domain.id)));let sql=rusqlite::Connection::open(&database).unwrap();
    let reject_writes=||sql.execute_batch("CREATE TRIGGER reject_overlay_write BEFORE INSERT ON meta BEGIN SELECT RAISE(ABORT,'unexpected overlay write'); END;").unwrap();
    let allow_writes=||sql.execute_batch("DROP TRIGGER reject_overlay_write;").unwrap();
    reject_writes();update_overlay(&store,&domain).unwrap();assert_eq!(serde_json::to_value(index::load_existing(&store.home,&domain.id,&binding).unwrap().unwrap()).unwrap(),serde_json::to_value(&j).unwrap());
    let mut queue=crate::smart::Queue::open(&store,&domain.id,&binding).unwrap();queue.enqueue(json!({"v":1,"op":"modify","domain":domain.id,"id":"file","request_id":"overlay-status","base_content_version":"content:file","base_metadata_version":"base","name":"renamed"}),None).unwrap();drop(queue);
    assert!(update_overlay(&store,&domain).unwrap_err().to_string().contains("unexpected overlay write"),"changed queue projection must attempt durable publication");allow_writes();update_overlay(&store,&domain).unwrap();
    let current=index::load_existing(&store.home,&domain.id,&binding).unwrap().unwrap();assert_eq!(current.items["file"].item.name,"renamed");assert_eq!(current.items["file"].item.sync_state.as_deref(),Some("queued"));let first_anchor=current.anchor;
    state::atomic_json(&native_cached_path(&store.home,&domain.id),&NativeCached{binding:binding.clone(),versions:BTreeMap::from([("file".into(),"content:file".into())])}).unwrap();update_overlay(&store,&domain).unwrap();
    let current=index::load_existing(&store.home,&domain.id,&binding).unwrap().unwrap();assert!(current.items["file"].item.native_materialized);assert!(current.anchor>first_anchor);let native_anchor=current.anchor;
    store.config.filters.exclude.push("*.tmp".into());store.save_config().unwrap();update_overlay(&store,&domain).unwrap();let current=index::load_existing(&store.home,&domain.id,&binding).unwrap().unwrap();assert!(current.anchor>native_anchor);let stable_anchor=current.anchor;
    assert_eq!(current.directory_turn,42);assert_eq!(current.completed_demands.get("retained-demand"),Some(&100));assert_eq!(current.requests["retained-receipt"].response,Some(json!({"kept":true})));
    sql.execute("UPDATE meta SET control_json=json_set(control_json,'$.pins_migrated',json('false'))",[]).unwrap();reject_writes();assert!(update_overlay(&store,&domain).is_err(),"pin normalization must not disappear behind the no-op fast path");allow_writes();update_overlay(&store,&domain).unwrap();let pins:i64=sql.query_row("SELECT json_extract(control_json,'$.pins_migrated') FROM meta",[],|r|r.get(0)).unwrap();assert_eq!(pins,1);
    sql.execute("UPDATE items SET record_json=json_set(record_json,'$.metadata_known',json('false')) WHERE id='file'",[]).unwrap();reject_writes();assert!(update_overlay(&store,&domain).is_err(),"metadata normalization must remain durable");allow_writes();update_overlay(&store,&domain).unwrap();let known:i64=sql.query_row("SELECT json_extract(record_json,'$.metadata_known') FROM items WHERE id='file'",[],|r|r.get(0)).unwrap();assert_eq!(known,1);
    let current=index::load_existing(&store.home,&domain.id,&binding).unwrap().unwrap();assert_eq!(current.anchor,stable_anchor,"read normalization must not invent item changes");reject_writes();update_overlay(&store,&domain).unwrap();allow_writes();
}
