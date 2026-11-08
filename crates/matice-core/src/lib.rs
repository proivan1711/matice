use anyhow::{Context, Result, bail};
use base64::prelude::*;
use krilla::{
    Data, Document,
    geom::{Size, Transform},
    image::Image,
    page::PageSettings,
};
use regex::Regex;
use reqwest_middleware::ClientBuilder;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{sync::Semaphore, task::JoinSet};

use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};

const JOBS: usize = 4;
const PADDING: f32 = 14.0;
//const URL: &str = "https://data.matricula-online.eu/en/BiH/banjaluka/bosansko-grahovo/01-01/?pg=1";

// A4 in portrait orientation, in points
const A4_WIDTH: f32 = 595.28;
const A4_HEIGHT: f32 = 841.89;

/// Pages finished so far out of the total. `total` stays 0 until the page list is fetched.
#[derive(Default)]
pub struct DownloadProgress {
    pub done: AtomicU64,
    pub total: AtomicU64,
}

pub async fn download(url: &str, path: PathBuf, progress: Arc<DownloadProgress>) {
    let metadata = get_metadata(url).await.unwrap();
    download_helper(metadata, path, progress).await;
}

struct Url {
    base64url: String,
    label: String,
}

pub struct DonwloadMetadata {
    urls: Vec<Url>,
}

async fn get_metadata(url: &str) -> Result<DonwloadMetadata> {
    let files_regex = Regex::new(r#""files": \[([^\]]+)\]"#).unwrap();
    let labels_raw_regex = Regex::new(r#""labels": \[([^\]]+)\]"#).unwrap();
    let base64_url_regex = Regex::new(r#""/image/([^"]+)/""#).unwrap();
    let labels_regex = Regex::new(r#""(.*?)""#).unwrap();

    let html = reqwest::get(url).await?.text().await?;

    let files = files_regex
        .captures(&html)
        .with_context(|| "Failed to extract base64 URLS")?
        .get(1)
        .with_context(|| "Failed to extract base64 URLS")?
        .as_str();

    let labels_raw = labels_raw_regex
        .captures(&html)
        .with_context(|| "Failed to extract labels")?
        .get(1)
        .with_context(|| "Failed to extract labels")?
        .as_str();

    let base64_urls: Vec<_> = base64_url_regex
        .captures_iter(files)
        .filter_map(|c| c.get(1))
        .map(|v| v.as_str().to_owned())
        .collect();

    let labels: Vec<_> = labels_regex
        .captures_iter(labels_raw)
        .filter_map(|c| c.get(1))
        .map(|v| v.as_str().to_owned())
        .collect();

    if base64_urls.len() != labels.len() {
        bail!(
            "URL links and labels mismatch: URLs: {}, URL labels: {}",
            base64_urls.len(),
            labels.len()
        );
    }

    let mut metadata = DonwloadMetadata { urls: vec![] };

    for i in 0..base64_urls.len() {
        metadata.urls.push(Url {
            base64url: base64_urls[i].to_string(),
            label: labels[i].to_string(),
        });
    }
    Ok(metadata)
}

async fn download_helper(
    metadata: DonwloadMetadata,
    path: PathBuf,
    progress: Arc<DownloadProgress>,
) {
    let mut tasks = JoinSet::new();

    // TODO:check if retry policy works
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    let client = Arc::new(
        ClientBuilder::new(reqwest::Client::new())
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build(),
    );
    let semaphore = Arc::new(Semaphore::new(JOBS));
    progress
        .total
        .store(metadata.urls.len() as u64, Ordering::Relaxed);

    for url in metadata.urls.into_iter() {
        let progress = progress.clone();
        let client = client.clone();
        let semaphore = semaphore.clone();
        let path = path.clone();

        tasks.spawn(async move {
            let _permit = semaphore.acquire().await?;
            let decoded_url = String::from_utf8(
                BASE64_STANDARD_NO_PAD
                    .decode(&url.base64url)
                    .with_context(|| "Failed to decode base64 URL")?,
            )?;
            let res = client.get(&decoded_url).send().await?.error_for_status()?;

            tokio::fs::write(path.join(format!("{}.pdf", url.label)), {
                let res_bytes = res.bytes().await?;
                tokio::task::spawn_blocking(|| img_to_pdf(res_bytes).unwrap()).await?
            })
            .await?;
            progress.done.fetch_add(1, Ordering::Relaxed);
            drop(_permit);

            Ok::<_, anyhow::Error>(())
        });
    }
    let tasks = tasks.join_all().await;
    for task in tasks {
        if let Err(err) = task {
            eprintln!("{}", err);
        }
    }
}

fn img_to_pdf(img_bytes: bytes::Bytes) -> Result<bytes::Bytes> {
    let data = Data::from(Vec::<u8>::from(img_bytes));
    let img = Image::from_jpeg(data, false).unwrap();

    let img_size = img.size();
    let img_aspect_ratio = img_size.0 as f32 / img_size.1 as f32;
    let is_horizontal = 1.0 < img_aspect_ratio;
    let (pdf_width, pdf_height) = if is_horizontal {
        (A4_HEIGHT, A4_WIDTH)
    } else {
        (A4_WIDTH, A4_HEIGHT)
    };

    let mut document = Document::new();
    let mut page = document.start_page_with(PageSettings::new(
        Size::from_wh(pdf_width, pdf_height).unwrap(),
    ));

    let max_img_width: f32 = pdf_width - PADDING * 2.0;
    let max_img_height: f32 = pdf_height - PADDING * 2.0;
    let img_size = fit_image(img_aspect_ratio, max_img_width, max_img_height);

    let img_x: f32 = PADDING + (max_img_width - img_size.0) / 2.0;
    let img_y: f32 = PADDING + (max_img_height - img_size.1) / 2.0;
    let mut surface = page.surface();

    surface.push_transform(&Transform::from_translate(img_x, img_y));
    surface.draw_image(img, Size::from_wh(img_size.0, img_size.1).unwrap());

    surface.pop();

    surface.finish();
    page.finish();

    Ok(bytes::Bytes::from(document.finish()?))
}

/// Largest size with the given aspect ratio (width / height) that fits in the box.
fn fit_image(aspect_ratio: f32, max_width: f32, max_height: f32) -> (f32, f32) {
    if aspect_ratio > max_width / max_height {
        (max_width, max_width / aspect_ratio)
    } else {
        (max_height * aspect_ratio, max_height)
    }
}
