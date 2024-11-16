use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::bili_client::BiliClient;
use crate::config::Config;
use crate::events;
use crate::events::{DownloadSpeedEvent, DownloadSpeedEventPayload};
use crate::extensions::AnyhowErrorToStringChain;
use crate::types::{AlbumPlusItem, ArchiveFormat, EpisodeInfo};

use aes::cipher::consts::U16;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes256;
use anyhow::{anyhow, Context};
use base64::engine::general_purpose;
use base64::Engine;
use byteorder::{BigEndian, ByteOrder};
use bytes::Bytes;
use parking_lot::RwLock;
use percent_encoding::percent_decode_str;
use reqwest::StatusCode;
use reqwest_middleware::ClientWithMiddleware;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use tauri::{AppHandle, Manager};
use tauri_specta::Event;
use tokio::sync::mpsc::Receiver;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;
use url::Url;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

// TODO: EpisodeInfo与AlbumPlusItem的内存差距过大，应该用Box包裹EpisodeInfo
enum DownloadPayload {
    Episode(EpisodeInfo),
    AlbumPlus(AlbumPlusItem),
}

/// 用于管理下载任务
///
/// 克隆 `DownloadManager` 的开销极小，性能开销几乎可以忽略不计。
/// 可以放心地在多个线程中传递和使用它的克隆副本。
///
/// 具体来说：
/// - `app` 是 `AppHandle` 类型，根据 `Tauri` 文档，它的克隆开销是极小的。
/// - 其他字段都被 `Arc` 包裹，这些字段的克隆操作仅仅是增加引用计数。
#[derive(Clone)]
pub struct DownloadManager {
    app: AppHandle,
    sender: Arc<mpsc::Sender<DownloadPayload>>,
    ep_sem: Arc<Semaphore>,
    img_sem: Arc<Semaphore>,
    byte_per_sec: Arc<AtomicU64>,
    downloaded_image_count: Arc<AtomicU32>,
    total_image_count: Arc<AtomicU32>,
}

impl DownloadManager {
    pub fn new(app: &AppHandle) -> Self {
        let (sender, receiver) = mpsc::channel::<DownloadPayload>(32);

        let (episode_concurrency, image_concurrency) = {
            let config = app.state::<RwLock<Config>>();
            let config = config.read();
            (config.episode_concurrency, config.image_concurrency)
        };
        let ep_sem = Arc::new(Semaphore::new(episode_concurrency));
        let img_sem = Arc::new(Semaphore::new(image_concurrency));

        let manager = DownloadManager {
            app: app.clone(),
            sender: Arc::new(sender),
            ep_sem,
            img_sem,
            byte_per_sec: Arc::new(AtomicU64::new(0)),
            downloaded_image_count: Arc::new(AtomicU32::new(0)),
            total_image_count: Arc::new(AtomicU32::new(0)),
        };

        tauri::async_runtime::spawn(Self::log_download_speed(app.clone()));
        tauri::async_runtime::spawn(Self::receiver_loop(app.clone(), receiver));

        manager
    }

    pub async fn submit_episode(&self, ep_info: EpisodeInfo) -> anyhow::Result<()> {
        let value = DownloadPayload::Episode(ep_info);
        self.sender.send(value).await?;
        Ok(())
    }

    pub async fn submit_album_plus(&self, item: AlbumPlusItem) -> anyhow::Result<()> {
        let value = DownloadPayload::AlbumPlus(item);
        self.sender.send(value).await?;
        Ok(())
    }

    pub fn set_episode_concurrency(&mut self, concurrency: usize) {
        self.ep_sem = Arc::new(Semaphore::new(concurrency));
    }

    pub fn set_image_concurrency(&mut self, concurrency: usize) {
        self.img_sem = Arc::new(Semaphore::new(concurrency));
    }

    #[allow(clippy::cast_precision_loss)]
    // TODO: 换个函数名，如emit_download_speed_loop
    async fn log_download_speed(app: AppHandle) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));

        loop {
            interval.tick().await;
            let manager = app.state::<RwLock<DownloadManager>>();
            let manager = manager.read();
            let byte_per_sec = manager.byte_per_sec.swap(0, Ordering::Relaxed);
            let mega_byte_per_sec = byte_per_sec as f64 / 1024.0 / 1024.0;
            let speed = format!("{mega_byte_per_sec:.2} MB/s");
            emit_download_speed_event(&app, speed);
        }
    }

    async fn receiver_loop(app: AppHandle, mut receiver: Receiver<DownloadPayload>) {
        while let Some(payload) = receiver.recv().await {
            let manager = app.state::<RwLock<DownloadManager>>().read().clone();
            match payload {
                DownloadPayload::Episode(ep_info) => {
                    tauri::async_runtime::spawn(manager.process_episode(ep_info));
                }
                DownloadPayload::AlbumPlus(item) => {
                    tauri::async_runtime::spawn(manager.process_album_plus(item));
                }
            }
        }
    }

    // TODO: 这里不应该返回错误，否则会被忽略
    #[allow(clippy::cast_possible_truncation)]
    async fn process_episode(self, ep_info: EpisodeInfo) -> anyhow::Result<()> {
        emit_pending_event(&self.app, ep_info.episode_id, ep_info.episode_title.clone());

        let http_client = create_http_client();
        let bili_client = self.bili_client();
        let image_index_resp_data = bili_client.get_image_index(ep_info.episode_id).await?;
        let urls: Vec<String> = image_index_resp_data
            .images
            .iter()
            .map(|img| img.path.clone())
            .collect();
        let image_token_data_data = bili_client.get_image_token(&urls, true).await?;

        let temp_download_dir = get_ep_temp_download_dir(&self.app, &ep_info);
        std::fs::create_dir_all(&temp_download_dir)
            .context(format!("创建目录 {temp_download_dir:?} 失败"))?;
        // 构造图片下载链接
        let urls: Vec<String> = image_token_data_data
            .into_iter()
            .map(|data| data.complete_url)
            .collect();
        let total = urls.len() as u32;
        // 记录总共需要下载的图片数量
        self.total_image_count.fetch_add(total, Ordering::Relaxed);
        let current = Arc::new(AtomicU32::new(0));
        let mut join_set = JoinSet::new();
        // 限制同时下载的章节数量
        let permit = self.ep_sem.acquire().await?;
        emit_start_event(
            &self.app,
            ep_info.episode_id,
            ep_info.episode_title.clone(),
            total,
        );

        for (i, url) in urls.iter().enumerate() {
            let http_client = http_client.clone();
            let manager = self.clone();
            let url = url.clone();
            let save_path = temp_download_dir.join(format!("{:03}.jpg", i + 1));
            let ep_id = ep_info.episode_id;
            let current = current.clone();
            // 创建下载任务
            join_set.spawn(manager.download_image(http_client, url, save_path, ep_id, current));
        }
        // 逐一处理完成的下载任务
        while let Some(completed_task) = join_set.join_next().await {
            completed_task?;
            self.downloaded_image_count.fetch_add(1, Ordering::Relaxed);
            let downloaded_image_count = self.downloaded_image_count.load(Ordering::Relaxed);
            let total_image_count = self.total_image_count.load(Ordering::Relaxed);
            // 更新下载进度
            emit_update_overall_progress_event(
                &self.app,
                downloaded_image_count,
                total_image_count,
            );
        }
        // 等待一段时间
        let episode_download_interval = self
            .app
            .state::<RwLock<Config>>()
            .read()
            .episode_download_interval;
        tokio::time::sleep(Duration::from_secs(episode_download_interval)).await;
        // 然后才继续下载下一章节
        drop(permit);
        // 如果DownloadManager所有图片全部都已下载(无论成功或失败)，则清空下载进度
        let downloaded_image_count = self.downloaded_image_count.load(Ordering::Relaxed);
        let total_image_count = self.total_image_count.load(Ordering::Relaxed);
        if downloaded_image_count == total_image_count {
            self.downloaded_image_count.store(0, Ordering::Relaxed);
            self.total_image_count.store(0, Ordering::Relaxed);
        }
        // 检查此章节的图片是否全部下载成功
        let current = current.load(Ordering::Relaxed);
        // 此章节的图片未全部下载成功
        if current != total {
            let err_msg = Some(format!("总共有 {total} 张图片，但只下载了 {current} 张"));
            emit_end_event(&self.app, ep_info.episode_id, err_msg);
            return Ok(());
        }
        // 此章节的图片全部下载成功
        let err_msg = match self.save_archive(&ep_info, &temp_download_dir) {
            Ok(_) => None,
            Err(err) => Some(err.to_string_chain()),
        };
        emit_end_event(&self.app, ep_info.episode_id, err_msg);
        Ok(())
    }

    fn save_archive(
        &self,
        ep_info: &EpisodeInfo,
        temp_download_dir: &PathBuf,
    ) -> anyhow::Result<()> {
        let archive_format = self
            .app
            .state::<RwLock<Config>>()
            .read()
            .archive_format
            .clone();

        let Some(parent) = temp_download_dir.parent() else {
            let err_msg = Some(format!("无法获取 {temp_download_dir:?} 的父目录"));
            emit_end_event(&self.app, ep_info.episode_id, err_msg);
            return Ok(());
        };

        let download_dir = parent.join(&ep_info.episode_title);
        // TODO: 把每种格式的保存操作提取到一个函数里
        match archive_format {
            ArchiveFormat::Image => {
                if download_dir.exists() {
                    std::fs::remove_dir_all(&download_dir)
                        .context(format!("删除 {download_dir:?} 失败"))?;
                }

                std::fs::rename(temp_download_dir, &download_dir).context(format!(
                    "将 {temp_download_dir:?} 重命名为 {download_dir:?} 失败"
                ))?;
            }
            ArchiveFormat::Cbz | ArchiveFormat::Zip => {
                let comic_info_path = temp_download_dir.join("ComicInfo.xml");
                let comic_info_xml = yaserde::ser::to_string(&ep_info.comic_info)
                    .map_err(|err_msg| anyhow!("序列化 {comic_info_path:?} 失败: {err_msg}"))?;
                std::fs::write(&comic_info_path, comic_info_xml)
                    .context(format!("创建 {comic_info_path:?} 失败"))?;

                let zip_path = download_dir.with_extension(archive_format.extension());
                let zip_file =
                    File::create(&zip_path).context(format!("创建 {zip_path:?} 失败"))?;

                let mut zip_writer = ZipWriter::new(zip_file);

                for entry in std::fs::read_dir(temp_download_dir)?.filter_map(Result::ok) {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }

                    let filename = match path.file_name() {
                        Some(name) => name.to_string_lossy(),
                        None => continue,
                    };

                    zip_writer
                        .start_file(&filename, SimpleFileOptions::default())
                        .context(format!("在 {zip_path:?} 创建 {filename:?} 失败"))?;

                    let mut file = File::open(&path).context(format!("打开 {path:?} 失败"))?;

                    std::io::copy(&mut file, &mut zip_writer)
                        .context(format!("将 {path:?} 写入 {zip_path:?} 失败"))?;
                }

                zip_writer
                    .finish()
                    .context(format!("关闭 {zip_path:?} 失败"))?;

                std::fs::remove_dir_all(temp_download_dir)
                    .context(format!("删除 {temp_download_dir:?} 失败"))?;
            }
        }
        Ok(())
    }

    // TODO: 这里不应该返回错误，否则会被忽略
    async fn process_album_plus(self, album_plus_item: AlbumPlusItem) -> anyhow::Result<()> {
        emit_pending_event(&self.app, album_plus_item.id, album_plus_item.title.clone());

        let http_client = create_http_client();
        let bili_client = self.bili_client();
        let image_token_data_data = bili_client
            .get_image_token(&album_plus_item.pic, false)
            .await?;

        let temp_download_dir = get_album_plus_temp_download_dir(&self.app, &album_plus_item);
        std::fs::create_dir_all(&temp_download_dir)
            .context(format!("创建目录 {temp_download_dir:?} 失败"))?;
        // 构造图片下载链接
        let urls: Vec<String> = image_token_data_data
            .into_iter()
            .map(|data| data.complete_url)
            .collect();
        let total = urls.len() as u32;
        // 记录总共需要下载的图片数量
        self.total_image_count.fetch_add(total, Ordering::Relaxed);
        let current = Arc::new(AtomicU32::new(0));
        let mut join_set = JoinSet::new();
        // 限制同时下载的章节数量
        let permit = self.ep_sem.acquire().await?;
        emit_start_event(
            &self.app,
            album_plus_item.id,
            album_plus_item.title.clone(),
            total,
        );
        for (i, url) in urls.iter().enumerate() {
            let http_client = http_client.clone();
            let manager = self.clone();
            let url = url.clone();
            let save_path = temp_download_dir.join(format!("{:03}.jpg", i + 1));
            let album_plus_id = album_plus_item.id;
            let current = current.clone();
            // 创建下载任务
            join_set.spawn(manager.download_image(
                http_client,
                url,
                save_path,
                album_plus_id,
                current,
            ));
        }
        // 逐一处理完成的下载任务
        while let Some(completed_task) = join_set.join_next().await {
            completed_task?;
            self.downloaded_image_count.fetch_add(1, Ordering::Relaxed);
            let downloaded_image_count = self.downloaded_image_count.load(Ordering::Relaxed);
            let total_image_count = self.total_image_count.load(Ordering::Relaxed);
            // 更新下载进度
            emit_update_overall_progress_event(
                &self.app,
                downloaded_image_count,
                total_image_count,
            );
        }
        drop(permit);
        // 如果DownloadManager所有图片全部都已下载(无论成功或失败)，则清空下载进度
        let downloaded_image_count = self.downloaded_image_count.load(Ordering::Relaxed);
        let total_image_count = self.total_image_count.load(Ordering::Relaxed);
        if downloaded_image_count == total_image_count {
            self.downloaded_image_count.store(0, Ordering::Relaxed);
            self.total_image_count.store(0, Ordering::Relaxed);
        }
        // 检查此章节的图片是否全部下载成功
        // TODO: 重构下面的代码
        let current = current.load(Ordering::Relaxed);
        if current == total {
            // 下载成功，则把临时目录重命名为正式目录
            if let Some(parent) = temp_download_dir.parent() {
                let download_dir = parent.join(&album_plus_item.title);
                std::fs::rename(&temp_download_dir, &download_dir).context(format!(
                    "将 {temp_download_dir:?} 重命名为 {download_dir:?} 失败"
                ))?;
            }
            emit_end_event(&self.app, album_plus_item.id, None);
        } else {
            let err_msg = Some(format!("总共有 {total} 张图片，但只下载了 {current} 张"));
            emit_end_event(&self.app, album_plus_item.id, err_msg);
        };
        Ok(())
    }

    // TODO: 把current变量名改成downloaded_count比较合适
    async fn download_image(
        self,
        http_client: ClientWithMiddleware,
        url: String,
        save_path: PathBuf,
        id: i64,
        current: Arc<AtomicU32>,
    ) {
        // 下载图片
        let permit = match self.img_sem.acquire().await.map_err(anyhow::Error::from) {
            Ok(permit) => permit,
            Err(err) => {
                let err = err.context("获取下载图片的semaphore失败");
                emit_error_event(&self.app, id, url, err.to_string_chain());
                return;
            }
        };
        let image_data = match get_image_bytes(http_client, &url).await {
            Ok(data) => data,
            Err(err) => {
                let err = err.context(format!("下载图片 {url} 失败"));
                emit_error_event(&self.app, id, url, err.to_string_chain());
                return;
            }
        };
        drop(permit);
        let parsed_url = match Url::parse(&url).map_err(anyhow::Error::from) {
            Ok(parsed_url) => parsed_url,
            Err(err) => {
                let err = err.context(format!("解析图片链接 {url} 失败"));
                emit_error_event(&self.app, id, url, err.to_string_chain());
                return;
            }
        };
        // 如果 parsed_url 里能找到cpx参数，则解密图片数据，否则用原始数据
        let image_data = match parsed_url.query_pairs().find(|(key, _)| key == "cpx") {
            Some((_, cpx)) => match decrypt_img_data(image_data, &cpx) {
                Ok(data) => data,
                Err(err) => {
                    let err = err.context(format!("解密图片 {url} 失败"));
                    emit_error_event(&self.app, id, url, err.to_string_chain());
                    return;
                }
            },
            None => image_data,
        };
        // 保存图片
        if let Err(err) = std::fs::write(&save_path, &image_data).map_err(anyhow::Error::from) {
            let err = err.context(format!("保存图片 {save_path:?} 失败"));
            emit_error_event(&self.app, id, url, err.to_string_chain());
            return;
        }
        // 记录下载字节数
        self.byte_per_sec
            .fetch_add(image_data.len() as u64, Ordering::Relaxed);
        // 更新章节下载进度
        let current = current.fetch_add(1, Ordering::Relaxed) + 1;
        emit_success_event(
            &self.app,
            id,
            save_path.to_string_lossy().to_string(), // TODO: 把save_path.to_string_lossy().to_string()保存到一个变量里，像current一样
            current,
        );
    }

    fn bili_client(&self) -> BiliClient {
        self.app.state::<BiliClient>().inner().clone()
    }
}

fn get_ep_temp_download_dir(app: &AppHandle, ep_info: &EpisodeInfo) -> PathBuf {
    app.state::<RwLock<Config>>()
        .read()
        .download_dir
        .join(&ep_info.comic_title)
        .join(format!(".下载中-{}", ep_info.episode_title)) // 以 `.下载中-` 开头，表示是临时目录
}

fn get_album_plus_temp_download_dir(app: &AppHandle, album_plus_item: &AlbumPlusItem) -> PathBuf {
    app.state::<RwLock<Config>>()
        .read()
        .download_dir
        .join(&album_plus_item.comic_title)
        .join("特典")
        .join(format!(".下载中-{}", album_plus_item.title)) // 以 `.下载中-` 开头，表示是临时目录
}

fn emit_start_event(app: &AppHandle, id: i64, title: String, total: u32) {
    let payload = events::DownloadStartEventPayload { id, title, total };
    let event = events::DownloadStartEvent(payload);
    let _ = event.emit(app);
}

fn emit_pending_event(app: &AppHandle, id: i64, title: String) {
    let payload = events::DownloadPendingEventPayload { id, title };
    let event = events::DownloadPendingEvent(payload);
    let _ = event.emit(app);
}

fn emit_success_event(app: &AppHandle, id: i64, url: String, current: u32) {
    let payload = events::DownloadImageSuccessEventPayload { id, url, current };
    let event = events::DownloadImageSuccessEvent(payload);
    let _ = event.emit(app);
}

fn emit_error_event(app: &AppHandle, id: i64, url: String, err_msg: String) {
    let payload = events::DownloadImageErrorEventPayload { id, url, err_msg };
    let event = events::DownloadImageErrorEvent(payload);
    let _ = event.emit(app);
}

fn emit_end_event(app: &AppHandle, id: i64, err_msg: Option<String>) {
    let payload = events::DownloadEndEventPayload { id, err_msg };
    let event = events::DownloadEndEvent(payload);
    let _ = event.emit(app);
}

#[allow(clippy::cast_lossless)]
fn emit_update_overall_progress_event(
    app: &AppHandle,
    downloaded_image_count: u32,
    total_image_count: u32,
) {
    let percentage: f64 = downloaded_image_count as f64 / total_image_count as f64 * 100.0;
    let payload = events::UpdateOverallDownloadProgressEventPayload {
        downloaded_image_count,
        total_image_count,
        percentage,
    };
    let event = events::UpdateOverallDownloadProgressEvent(payload);
    let _ = event.emit(app);
}

fn emit_download_speed_event(app: &AppHandle, speed: String) {
    let payload = DownloadSpeedEventPayload { speed };
    let event = DownloadSpeedEvent(payload);
    let _ = event.emit(app);
}

async fn get_image_bytes(http_client: ClientWithMiddleware, url: &str) -> anyhow::Result<Bytes> {
    // 发送下载图片请求
    let http_resp = http_client.get(url).send().await?;
    // 检查http响应状态码
    let status = http_resp.status();
    if status != StatusCode::OK {
        let body = http_resp.text().await?;
        return Err(anyhow!("下载图片 {url} 失败，预料之外的状态码: {body}"));
    }
    // 读取图片数据
    let image_data = http_resp.bytes().await?;

    Ok(image_data)
}

fn create_http_client() -> ClientWithMiddleware {
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(2);

    reqwest_middleware::ClientBuilder::new(reqwest::Client::new())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build()
}

fn aes_cbc_decrypt(encrypted_data: &[u8], key: &[u8], iv: &[u8]) -> Vec<u8> {
    const BLOCK_SIZE: usize = 16;
    let cipher = Aes256::new(GenericArray::from_slice(key));
    // 存储解密后的数据
    let mut decrypted_data_with_padding = Vec::with_capacity(encrypted_data.len());
    // 将IV作为初始化块处理，解密时与第一个加密块进行异或
    let mut previous_block: GenericArray<u8, U16> = *GenericArray::from_slice(iv);
    // 逐块解密
    for chunk in encrypted_data.chunks(BLOCK_SIZE) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        // 与前一个块进行异或操作
        for i in 0..BLOCK_SIZE {
            block[i] ^= previous_block[i]; // 与前一个块进行异或
        }
        // 将解密后的数据追加到解密结果
        decrypted_data_with_padding.extend_from_slice(&block);
        // 将当前块的密文作为下一个块的IV
        previous_block = GenericArray::clone_from_slice(chunk);
    }

    // 去除PKCS#7填充，根据最后一个字节的值确定填充长度
    let padding_len = decrypted_data_with_padding.last().copied().unwrap() as usize;
    let data_len = decrypted_data_with_padding.len() - padding_len;
    decrypted_data_with_padding[..data_len].to_vec()
}

fn decrypt_img_data(img_data: Bytes, cpx: &str) -> anyhow::Result<Bytes> {
    // 如果数据能够被解析为图片格式，则直接返回
    if image::guess_format(&img_data).is_ok() {
        return Ok(img_data);
    }
    // 否则，解密图片数据
    let img_flag = img_data[0];
    if img_flag != 1 {
        return Err(anyhow!(
            "解密图片数据失败，预料之外的图片数据标志位: {img_flag}"
        ));
    }
    let data_length = BigEndian::read_u32(&img_data[1..5]) as usize;
    if data_length + 5 > img_data.len() {
        return Ok(img_data);
    };
    // 准备解密所需的数据
    let cpx_text = percent_decode_str(cpx).decode_utf8_lossy().to_string();
    let cpx_char = general_purpose::STANDARD.decode(cpx_text)?;
    let iv = &cpx_char[60..76];
    let key = &img_data[data_length + 5..];
    let content = &img_data[5..data_length + 5];
    // 如果数据长度小于20496，则直接解密
    if content.len() < 20496 {
        let decrypted_data = aes_cbc_decrypt(content, key, iv);
        return Ok(decrypted_data.into());
    }
    // 否则，先解密前20496字节，再拼接后面的数据
    let img_head = aes_cbc_decrypt(&content[0..20496], key, iv);
    let mut decrypted_data = img_head;
    decrypted_data.extend_from_slice(&content[20496..]);
    Ok(decrypted_data.into())
}
