use crate::client::StatelessClient;

use crate::downloader::extractor::{find_extractor, SiteDefinition};
use crate::downloader::util::{LifecycleFile, Segmentable};
use crate::server::core::live_streamers::{DynLiveStreamersService, LiveStreamerDto};
use crate::server::core::upload_actor::UploadActorHandle;
use crate::server::core::util::{logging_spawn, AnyMap, Cycle};
use crate::server::core::StreamStatus;

use indexmap::indexmap;

use crate::server::core::upload_streamers::DynUploadStreamersRepository;
use std::collections::HashMap;
use std::error::Error;
use std::ops::DerefMut;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, error};

async fn start_monitor(
    task: Cycle<StreamStatus>,
    extractor: &(dyn SiteDefinition + Send + Sync),
    client: StatelessClient,
    live_streamers_service: DynLiveStreamersService,
) {
    let n = &mut 0;
    loop {
        let (url, status) = task.get(n);
        match (extractor.get_site(&url, client.clone()).await, status) {
            (Ok(mut site), StreamStatus::Idle) => {
                println!("Idle\n {url} \n{site}");
                let client = client.clone();
                let url_c = url.clone();
                let live_streamers_service = live_streamers_service.clone();
                logging_spawn(async move {
                    let mut file = LifecycleFile::new("./video/%Y-%m-%d/%H_%M_%S{title}");
                    if let Some(studio) = live_streamers_service.get_studio_by_url(&url_c).await? {
                        let handle = UploadActorHandle::new(client, studio);
                        file.hook = Box::new(move |file_name| {
                            match std::fs::metadata(file_name) {
                                Ok(metadata) => {
                                    if metadata.len() > 10 * 1024 * 1024 {
                                        println!("开始上传");
                                        handle.send_file_path(file_name);
                                    }
                                }
                                Err(error) => {
                                    error!("{}", error)
                                }
                            }
                            println!("tick{file_name}")
                        });
                    } else {
                        error!(url = %url_c, "upload template not set.")
                    }
                    let segmentable = Segmentable::new(Some(Duration::from_secs(60)), None);
                    // let segmentable = Segmentable::new( None, Some(16*1024*1024));
                    site.download(file, segmentable).await?;
                    Ok::<_, Box<dyn Error + Send + Sync>>(())
                });
                task.write()
                    .entry(url)
                    .and_modify(|status| *status = StreamStatus::Downloading);
            }
            (Ok(_site), StreamStatus::Downloading) => {
                println!("Downloading {url}");
            }
            (Ok(_site), StreamStatus::Pending) => {
                println!("Pending");
            }
            (Ok(_site), StreamStatus::Uploading) => {
                println!("Uploading");
            }
            (Err(e), _) => {
                debug!(url, "{e}")
            }
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

struct DownloadActor {
    live_streamers_service: DynLiveStreamersService,
    client: StatelessClient,
}

impl DownloadActor {
    fn new(live_streamers_service: DynLiveStreamersService, client: StatelessClient) -> Self {
        Self {
            live_streamers_service,
            client,
        }
    }

    fn run(
        &mut self,
        list: Vec<LiveStreamerDto>,
        extensions: StreamActorMap,
        // client: StatelessClient,
    ) {
        for streamer in list {
            // let Some(extractor) = find_extractor(&streamer.url) else { continue; };
            let mut guard = extensions.write().unwrap();
            self.add_streamer(guard.deref_mut(), streamer.url)
        }
        println!("{:?}", extensions);
    }

    fn add_streamer(
        &self,
        map: &mut AnyMap<(Cycle<StreamStatus>, JoinHandle<()>)>,
        url: String,
        // client: StatelessClient,
    ) {
        let Some(extractor) = find_extractor(&url) else { return; };
        let _entry = map
            .entry(extractor.as_any().type_id())
            .and_modify(|(cy, _)| cy.insert(url.clone(), StreamStatus::Idle))
            .or_insert_with(|| {
                let cycle = Cycle::new(indexmap![url => StreamStatus::Idle]);
                let task = cycle.clone();
                let client = self.client.clone();
                let live_streamers_service = self.live_streamers_service.clone();
                let handle = tokio::spawn(async move {
                    start_monitor(task, extractor, client, live_streamers_service).await
                });
                (cycle, handle)
            });
    }
}

type StreamActorMap = Arc<RwLock<AnyMap<(Cycle<StreamStatus>, JoinHandle<()>)>>>;

pub struct DownloadActorHandle {
    platform_map: StreamActorMap,
    // client: StatelessClient,
    actor: DownloadActor,
}

impl DownloadActorHandle {
    pub fn new(
        list: Vec<LiveStreamerDto>,
        client: StatelessClient,
        live_streamers_service: DynLiveStreamersService,
    ) -> Self {
        let mut actor = DownloadActor::new(live_streamers_service, client);
        let platform_map = Arc::new(RwLock::new(HashMap::default()));
        let platform = Arc::clone(&platform_map);
        // let client_c = client.clone();
        actor.run(list, platform);
        Self {
            platform_map,
            actor,
        }
    }

    pub fn add_streamer(&self, url: &str) {
        self.actor.add_streamer(
            self.platform_map.write().unwrap().deref_mut(),
            url.to_string(),
            // self.client.clone(),
        );
    }

    pub fn remove_streamer(&self, url: &str) {
        find_extractor(url).and_then(|extractor| {
            self.platform_map
                .read()
                .unwrap()
                .get(&extractor.as_any().type_id())
                .and_then(|(cy, join_handle)| {
                    let mut guard = cy.write();
                    if guard.len() <= 1 {
                        join_handle.abort()
                    }
                    guard.remove(url)
                })
        });
    }
}
