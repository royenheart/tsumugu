use std::{
    collections::HashSet,
    fs::File,
    io::Write,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use chrono::{FixedOffset, NaiveDateTime};
use crossbeam_deque::{Injector, Worker};
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use tracing::{debug, error, info, warn};
use url::Url;

use crate::{
    build_client,
    compare::{should_download_by_head, should_download_by_list},
    extensions::{extension_handler, ExtensionPackage},
    listing::{self, ListItem},
    parser::ListResult,
    regex_process::{self, ExclusionManager},
    term::AlternativeTerm,
    utils::{self, again, again_async, get_async, head, is_symlink, naive_to_utc},
    SyncArgs,
};

#[derive(Debug, Clone)]
enum TaskType {
    Listing,
    Download(ListItem),
}

#[derive(Debug, Clone)]
struct Task {
    task: TaskType,
    relative: Vec<String>,
    url: Url,
}

fn extension_push_task(worker: &Worker<Task>, wake: &AtomicUsize, package: &ExtensionPackage) {
    worker.push(Task {
        task: TaskType::Download(ListItem {
            url: package.url.clone(),
            name: package.filename.clone(),
            type_: listing::FileType::File,
            // size and mtime would be ignored as skip_check is set
            size: None,
            mtime: NaiveDateTime::default(),
            skip_check: true,
        }),
        relative: package.relative.clone(),
        url: package.url.clone(),
    });
    wake.fetch_add(1, Ordering::SeqCst);
}

fn determinate_timezone(
    args: &SyncArgs,
    parser: &dyn crate::parser::Parser,
    client: &reqwest::blocking::Client,
) -> Option<FixedOffset> {
    match args.timezone {
        None => {
            // Check if to guess timezone
            let timezone_file = match &args.timezone_file {
                Some(f) => match Url::parse(f) {
                    Ok(url) => Some(url),
                    Err(_) => {
                        warn!("Invalid timezone file URL, disabling timezone guessing");
                        None
                    }
                },
                None => {
                    // eek, try getting first file in root index
                    let list =
                        again(|| parser.get_list(client, &args.upstream), args.retry).unwrap();
                    match list {
                        ListResult::List(list) => {
                            match list.iter().find(|x| x.type_ == listing::FileType::File) {
                                None => {
                                    warn!("No files in root index, disabling timezone guessing");
                                    None
                                }
                                Some(x) => Some(x.url.clone()),
                            }
                        }
                        ListResult::Redirect(_) => {
                            warn!("Root index is a redirect, disabling timezone guessing");
                            None
                        }
                    }
                }
            };
            match timezone_file {
                Some(timezone_url) => {
                    let timezone = listing::guess_remote_timezone(parser, client, timezone_url);
                    let timezone = match timezone {
                        Ok(tz) => Some(tz),
                        Err(e) => {
                            warn!("Failed to guess timezone: {:?}", e);
                            None
                        }
                    };
                    info!("Guessed timezone: {:?}", timezone);
                    timezone
                }
                None => None,
            }
        }
        Some(tz) => {
            info!("Using timezone from argument: {:?} hrs", tz);
            Some(FixedOffset::east_opt(tz * 3600).unwrap())
        }
    }
}

struct ThreadsOptions<'a> {
    bind_address: Option<String>,
    download_dir: &'a Path,
    remote_list: &'a Arc<Mutex<HashSet<PathBuf>>>,
    stat_objects: &'a AtomicUsize,
    stat_size: &'a AtomicU64,
    failure_listing: &'a AtomicBool,
    failure_downloading: &'a AtomicBool,
}

fn sync_threads(args: &SyncArgs, parser: &dyn crate::parser::Parser, options: &ThreadsOptions) {
    let exclusion_manager = ExclusionManager::new(&args.exclude, &args.include);

    let client = build_client!(
        reqwest::blocking::Client,
        args,
        parser,
        options.bind_address.as_ref()
    );
    // async support
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let async_client = build_client!(reqwest::Client, args, parser, options.bind_address.as_ref());

    let mprogress = MultiProgress::with_draw_target(ProgressDrawTarget::term_like_with_hz(
        Box::new(AlternativeTerm::buffered_stdout()),
        1,
    ));

    let timezone = determinate_timezone(args, parser, &client);

    if !args.dry_run {
        std::fs::create_dir_all(options.download_dir).unwrap();
    }

    let workers: Vec<_> = (0..args.threads)
        .map(|_| Worker::<Task>::new_fifo())
        .collect();
    let stealers: Vec<_> = workers.iter().map(|w| w.stealer()).collect();
    let global = Injector::<Task>::new();

    global.push(Task {
        task: TaskType::Listing,
        relative: vec![],
        url: args.upstream.clone(),
    });

    let active_cnt = AtomicUsize::new(0);
    let wake = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for worker in workers {
            scope.spawn(|| {
                loop {
                    active_cnt.fetch_add(1, Ordering::SeqCst);
                    while let Some(task) = worker.pop().or_else(|| {
                        std::iter::repeat_with(|| {
                            global
                                .steal_batch_and_pop(&worker)
                                .or_else(|| stealers.iter().map(|s| s.steal()).collect())
                        })
                        .find(|s| !s.is_retry())
                        .and_then(|s| s.success())
                    }) {
                        let relative = task.relative.join("/");
                        let cwd = options.download_dir.join(&relative);
                        debug!("cwd: {:?}, relative: {:?}", cwd, relative);
                        // exclude this?
                        // note that it only checks the relative folder!
                        // Downloading files will still be checked again.
                        let exclusion_result = exclusion_manager.match_str(&relative);
                        if exclusion_result == regex_process::Comparison::Stop {
                            info!("Skipping excluded {:?}", &relative);
                            continue;
                        } else if exclusion_result == regex_process::Comparison::ListOnly {
                            info!("List only in {:?}", &relative);
                        }
                        match task.task {
                            TaskType::Listing => {
                                info!("Listing {}", task.url);
                                {
                                    options.remote_list.lock().unwrap().insert(cwd.clone());
                                }

                                if is_symlink(&cwd) && !relative.is_empty() {
                                    info!("{:?} is a symlink, ignored", cwd);
                                    continue;
                                }

                                let items = match again(|| parser.get_list(&client, &task.url), args.retry) {
                                    Ok(items) => items,
                                    Err(e) => {
                                        error!("Failed to list {}: {:?}", task.url, e);
                                        options.failure_listing.store(true, Ordering::SeqCst);
                                        continue;
                                    }
                                };
                                match items {
                                    ListResult::List(items) => {
                                        for item in items {
                                            if item.type_ == listing::FileType::Directory {
                                                let mut relative = task.relative.clone();
                                                relative.push(item.name);
                                                worker.push(Task {
                                                    task: TaskType::Listing,
                                                    relative,
                                                    url: item.url,
                                                });
                                                wake.fetch_add(1, Ordering::SeqCst);
                                            } else {
                                                if exclusion_result == regex_process::Comparison::ListOnly {
                                                    info!("Skipping (by list only) {}", item.url);
                                                    continue;
                                                }
                                                worker.push(Task {
                                                    task: TaskType::Download(item.clone()),
                                                    relative: task.relative.clone(),
                                                    url: item.url,
                                                });
                                                wake.fetch_add(1, Ordering::SeqCst);
                                                options.stat_size.fetch_add(match item.size {
                                                    Some(size) => size.get_estimated(),
                                                    None => 0,
                                                }, Ordering::SeqCst);
                                            }
                                            options.stat_objects.fetch_add(1, Ordering::SeqCst);
                                        }
                                    }
                                    ListResult::Redirect(target_url) => {
                                        // This "Redirect" only supports creating symlink of current directory
                                        info!("Redirected {} -> {}. Try to create a symlink", task.url, target_url);
                                        if cwd.exists() {
                                            warn!("Skipping symlink creation because {:?} already exists, but it is not a symlink", cwd);
                                            continue;
                                        }
                                        // get last segment of target_url
                                        let target_name = match target_url.split('/').nth_back(1) {
                                            Some(name) => name,
                                            None => {
                                                error!("Failed to get last segment of target_url: {}", target_url);
                                                continue;
                                            }
                                        };
                                        info!("Try symlink {:?} -> {}", cwd, target_name);
                                        if let Err(e) = symlink(target_name, cwd.clone()) {
                                            error!("Failed to create symlink {:?} -> {}: {:?}", cwd, target_name, e);
                                        }
                                    }
                                }
                            }
                            TaskType::Download(item) => {
                                // create path in case for first sync
                                if !args.dry_run {
                                    std::fs::create_dir_all(&cwd).unwrap();
                                }
                                // Absolute filesystem path of expected file
                                let expected_path = cwd.join(&item.name);
                                // Here relative filepath is only used to check exclusion
                                let relative_filepath = PathBuf::from(&relative).join(&item.name);
                                let relative_filepath = relative_filepath.to_string_lossy();
                                debug!("expected_path: {:?}, relative: {:?}", expected_path, relative_filepath);
                                {
                                    if !options.remote_list.lock().unwrap().insert(expected_path.clone()) {
                                        // It is possible that multiple tasks might download the same file
                                        // (generated by apt/yum parser, etc.)
                                        // skip when we find that some thread has already downloaded it
                                        info!("Skipping already handled {:?}", &expected_path);
                                        continue;
                                    }
                                }

                                // We should put relative filepath into exclusion manager here
                                if exclusion_manager.match_str(&relative_filepath) == regex_process::Comparison::Stop {
                                    info!("Skipping excluded {:?}", &relative_filepath);
                                    continue;
                                }

                                let mut should_download = true;
                                let mut skip_if_exists = false;
                                for i in &args.skip_if_exists {
                                    if i.is_match(&relative_filepath) {
                                        skip_if_exists = true;
                                        break;
                                    }
                                }

                                // Following code requires real filesystem path (expected_path) to work
                                if !should_download_by_list(&expected_path, &item, timezone, skip_if_exists, false) {
                                    info!("Skipping {}", task.url);
                                    should_download = false;
                                }

                                let mut compare_size_only = false;
                                for i in &args.compare_size_only {
                                    if i.is_match(&expected_path.to_string_lossy()) {
                                        compare_size_only = true;
                                        break;
                                    }
                                }

                                if should_download && args.head_before_get {
                                    match again(|| head(&client, item.url.clone()), args.retry) {
                                        Ok(resp) => {
                                            if !should_download_by_head(&expected_path, &resp, compare_size_only) {
                                                info!("Skipping (by HEAD) {}", task.url);
                                                should_download = false;
                                            }
                                        },
                                        Err(e) => {
                                            error!("Failed to HEAD {}: {:?}", task.url, e);
                                            options.failure_downloading.store(true, Ordering::SeqCst);
                                            should_download = false;
                                        }
                                    };
                                }

                                if should_download && !args.dry_run {
                                    let future = async {
                                        // Here we use async to allow streaming and progress bar
                                        // Ref: https://gist.github.com/giuliano-oliveira/4d11d6b3bb003dba3a1b53f43d81b30d
                                        let resp = match again_async(|| get_async(&async_client, item.url.clone()), args.retry).await {
                                            Ok(resp) => resp,
                                            Err(e) => {
                                                error!("Failed to GET {}: {:?}", task.url, e);
                                                options.failure_downloading.store(true, Ordering::SeqCst);
                                                return;
                                            }
                                        };
                                        let total_size = resp.content_length().unwrap();
                                        let pb = mprogress.add(ProgressBar::new(total_size));
                                        pb.set_style(ProgressStyle::default_bar()
                                            .template("{msg}\n[{elapsed_precise}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})").unwrap()
                                            .progress_chars("#>-"));
                                        pb.set_message(format!("Downloading {}", item.url));

                                        let mtime = match utils::get_async_response_mtime(&resp) {
                                            Ok(mtime) => mtime,
                                            Err(e) => {
                                                if args.allow_mtime_from_parser {
                                                    naive_to_utc(&item.mtime, timezone)
                                                } else {
                                                error!("Failed to get mtime of {}: {:?}", task.url, e);
                                                options.failure_downloading.store(true, Ordering::SeqCst);
                                                return;
                                            }}
                                        };

                                        let tmp_path = cwd.join(format!(".tmp.{}", item.name));
                                        {
                                            let mut dest_file = File::create(&tmp_path).unwrap();
                                            let mut stream = resp.bytes_stream();

                                            while let Some(item) = stream.next().await {
                                                let chunk = item.unwrap();
                                                dest_file.write_all(&chunk).unwrap();
                                                let new = std::cmp::min(pb.position() + (chunk.len() as u64), total_size);
                                                pb.set_position(new);
                                            }
                                            filetime::set_file_handle_times(
                                                &dest_file,
                                                None,
                                                Some(filetime::FileTime::from_system_time(mtime.into())),
                                            )
                                            .unwrap();
                                        }
                                        // move tmp file to expected path
                                        std::fs::rename(&tmp_path, &expected_path).unwrap();
                                    };
                                    runtime.block_on(future);
                                } else if should_download {
                                    info!("Dry run, not downloading {}", task.url);
                                }

                                extension_handler(args, &expected_path, &task.relative, &item.url, |package| {
                                    extension_push_task(&worker, &wake, package);
                                });
                            }
                        }
                    }
                    let active = active_cnt.fetch_sub(1, Ordering::SeqCst);
                    if active == 1 {
                        // only self is active before this
                        break;
                    } else {
                        // sleep and wait for waking up
                        debug!("Sleep and wait for waking up");
                        loop {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                            let old_wake = wake.load(Ordering::SeqCst);
                            if old_wake > 0 {
                                let new_wake = old_wake - 1;
                                if wake
                                    .compare_exchange(
                                        old_wake,
                                        new_wake,
                                        Ordering::SeqCst,
                                        Ordering::SeqCst,
                                    )
                                    .is_ok()
                                {
                                    break;
                                }
                            }
                        }
                    }
                }
                info!("This thread finished");
                // drop worker to let rustc know it moves inside the closure
                std::mem::drop(worker);
            });
        }
    });
}

pub fn sync(args: &SyncArgs, bind_address: Option<String>) -> ! {
    debug!("{:?}", args);
    let parser = args.parser.build();

    let download_dir = args.local.as_path();

    let remote_list = Arc::new(Mutex::new(HashSet::new()));

    let stat_objects = AtomicUsize::new(0);
    let stat_size = AtomicU64::new(0);

    let failure_listing = AtomicBool::new(false);
    let failure_downloading = AtomicBool::new(false);

    sync_threads(
        args,
        &*parser,
        &ThreadsOptions {
            bind_address,
            download_dir,
            remote_list: &remote_list,
            stat_objects: &stat_objects,
            stat_size: &stat_size,
            failure_listing: &failure_listing,
            failure_downloading: &failure_downloading,
        },
    );

    let mut exit_code = 0;

    // Removing files that are not in remote list
    let mut del_cnt = 0;
    let remote_list = remote_list.lock().unwrap();
    if failure_listing.load(Ordering::SeqCst) {
        error!("Failed to list remote, not to delete anything");
        exit_code = 1;
    } else {
        // Don't even walkdir when dry_run, to prevent no dir error
        for entry in walkdir::WalkDir::new(download_dir).contents_first(true) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    error!("Failed to walkdir: {:?}", e);
                    if !args.dry_run {
                        exit_code = 1;
                    }
                    break;
                }
            };
            let path = entry.path();
            if !remote_list.contains(&path.to_path_buf()) {
                if args.no_delete {
                    info!("{:?} not in remote", path);
                } else {
                    // always make sure that we are deleting the right thing
                    if del_cnt >= args.max_delete {
                        info!("Exceeding max delete count, aborting");
                        // exit with 25 to indicate that the deletion has been aborted
                        // this is the same as rsync
                        exit_code = 25;
                        break;
                    }
                    del_cnt += 1;
                    assert!(path.starts_with(download_dir));
                    if args.dry_run {
                        info!("Dry run, not deleting {:?}", path);
                        continue;
                    }

                    info!("Deleting {:?}", path);
                    if entry.file_type().is_dir() {
                        if let Err(e) = std::fs::remove_dir(path) {
                            error!("Failed to remove {:?}: {:?}", path, e);
                            exit_code = 4;
                        }
                    } else if let Err(e) = std::fs::remove_file(path) {
                        error!("Failed to remove {:?}: {:?}", path, e);
                        exit_code = 4;
                    }
                }
            }
        }
    }

    if failure_downloading.load(Ordering::SeqCst) {
        error!("Failed to download some files");
        exit_code = 2;
    }

    // Show stat
    info!(
        "(Estimated) Total objects: {}, total size: {}",
        stat_objects.load(Ordering::SeqCst),
        humansize::format_size(stat_size.load(Ordering::SeqCst), humansize::BINARY)
    );

    std::process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    // use super::*;

    #[test]
    fn test_relative() {
        let mut relative: Vec<String> = vec![];
        assert_eq!(relative.join("/"), "");
        relative.push("debian".to_string());
        assert_eq!(relative.join("/"), "debian");
        relative.push("dists".to_string());
        assert_eq!(relative.join("/"), "debian/dists");
    }
}
