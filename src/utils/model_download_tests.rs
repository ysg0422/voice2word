//! 下载回归：真实 HTTP 响应、独立临时目录，不依赖外网或真实模型。
use super::*;
use crate::utils::TempPathGuard;
use std::net::TcpListener;
use std::thread;

pub(crate) fn workspace() -> TempPathGuard {
    let p = std::env::temp_dir().join(format!("v2w-dl-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&p).unwrap();
    TempPathGuard::dir(p)
}

pub(crate) fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!("HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n").into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

pub(crate) fn server(replies: Vec<Vec<u8>>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/model", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut requests = Vec::new();
        for reply in replies {
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "没有收到预期的下载请求");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut raw = Vec::new();
            while !raw.ends_with(b"\r\n\r\n") {
                let mut b = [0];
                if stream.read(&mut b).unwrap() == 0 {
                    break;
                }
                raw.push(b[0]);
            }
            requests.push(String::from_utf8(raw).unwrap());
            let _ = stream.write_all(&reply);
        }
        requests
    });
    (url, handle)
}

fn fetch(url: &str, path: &Path, size: u64) -> Result<()> {
    fetch_to_file(
        url,
        path,
        size,
        "test",
        &Arc::new(AtomicBool::new(false)),
        None,
        &mut 0,
    )
}

fn payload() -> Vec<u8> {
    let mut body = vec![7; 1000];
    body[..4].copy_from_slice(b"lmgg");
    body
}

fn item(dest: &Path, urls: Vec<String>, archive: bool) -> DownloadItem {
    let urls: Vec<&'static str> = urls
        .into_iter()
        .map(|s| &*Box::leak(s.into_boxed_str()))
        .collect();
    DownloadItem {
        id: "download-test",
        label: "test",
        note: "test",
        dest: Box::leak(dest.to_string_lossy().into_owned().into_boxed_str()),
        urls: Box::leak(urls.into_boxed_slice()),
        size: if archive { 1 } else { 1000 },
        min_size: 0,
        required: false,
        group: ItemGroup::Asr,
        is_archive: archive,
        companion: None,
        sha256: None,
    }
}

#[test]
fn interrupted_transfer_resumes_with_if_range_and_exact_total() {
    let dir = workspace();
    let part = dir.path().join("model.bin.part");
    let body = payload();
    let (url, served) = server(vec![
        response(
            "200 OK",
            "Content-Length: 1000\r\nETag: \"v1\"\r\n",
            &body[..970],
        ),
        response(
            "206 Partial Content",
            "Content-Length: 30\r\nContent-Range: bytes 970-999/1000\r\nETag: \"v1\"\r\n",
            &body[970..],
        ),
    ]);
    assert!(fetch(&url, &part, 1000).is_err());
    assert_eq!(fs::metadata(&part).unwrap().len(), 970);
    assert!(!read_resume_metadata(&part).unwrap().complete);
    fetch(&url, &part, 1000).unwrap();
    assert_eq!(fs::read(&part).unwrap(), body);
    let requests = served.join().unwrap();
    let request = requests[1].to_ascii_lowercase();
    assert!(request.contains("range: bytes=970-"));
    assert!(request.contains("if-range: \"v1\""));
    assert!(read_resume_metadata(&part).unwrap().complete);
}

#[test]
fn unverified_full_size_partial_is_downloaded_again() {
    let dir = workspace();
    let part = dir.path().join("model.bin.part");
    fs::write(&part, vec![0; 1000]).unwrap();
    let body = payload();
    let (url, served) = server(vec![response("200 OK", "Content-Length: 1000\r\n", &body)]);
    fetch(&url, &part, 1000).unwrap();
    assert_eq!(fs::read(&part).unwrap(), body);
    assert_eq!(served.join().unwrap().len(), 1);
}

#[test]
fn changed_etag_and_invalid_ranges_restart_instead_of_splicing() {
    for headers in [
        "Content-Length: 500\r\nContent-Range: bytes 500-999/1000\r\nETag: \"v2\"\r\n",
        "Content-Length: 500\r\nContent-Range: bytes 0-499/1000\r\nETag: \"v1\"\r\n",
        "Content-Length: 500\r\nContent-Range: bytes 500-/1000\r\nETag: \"v1\"\r\n",
    ] {
        let dir = workspace();
        let part = dir.path().join("model.bin.part");
        let body = payload();
        let (url, served) = server(vec![
            response("206 Partial Content", headers, &vec![3; 500]),
            response("200 OK", "Content-Length: 1000\r\nETag: \"v2\"\r\n", &body),
        ]);
        fs::write(&part, vec![1; 500]).unwrap();
        write_resume_metadata(
            &part,
            &ResumeMetadata {
                url: url.clone(),
                validator: Some("\"v1\"".into()),
                total: 1000,
                complete: false,
            },
        )
        .unwrap();
        fetch(&url, &part, 1000).unwrap();
        assert_eq!(fs::read(&part).unwrap(), body);
        let requests = served.join().unwrap();
        assert!(!requests[1].to_ascii_lowercase().contains("\r\nrange:"));
    }
}

#[test]
fn changing_mirror_never_appends_an_unrelated_partial() {
    let dir = workspace();
    let part = dir.path().join("model.bin.part");
    fs::write(&part, vec![1; 500]).unwrap();
    write_resume_metadata(
        &part,
        &ResumeMetadata {
            url: "http://old.invalid/model".into(),
            validator: Some("\"v1\"".into()),
            total: 1000,
            complete: false,
        },
    )
    .unwrap();
    let body = payload();
    let (url, served) = server(vec![response("200 OK", "Content-Length: 1000\r\n", &body)]);
    fetch(&url, &part, 1000).unwrap();
    assert_eq!(fs::read(&part).unwrap(), body);
    assert!(!served.join().unwrap()[0]
        .to_ascii_lowercase()
        .contains("\r\nrange:"));
}

#[test]
fn http_200_with_a_partial_content_range_is_not_treated_as_a_complete_file() {
    let dir = workspace();
    let part = dir.path().join("model.onnx.part");
    let body = payload();
    let (url, served) = server(vec![
        response(
            "200 OK",
            "Content-Length: 990\r\nContent-Range: bytes 10-999/1000\r\nETag: \"v1\"\r\n",
            &body[10..],
        ),
        response("200 OK", "Content-Length: 1000\r\nETag: \"v1\"\r\n", &body),
    ]);
    fs::write(&part, &body[..10]).unwrap();
    write_resume_metadata(
        &part,
        &ResumeMetadata {
            url: url.clone(),
            validator: Some("\"v1\"".into()),
            total: 1000,
            complete: false,
        },
    )
    .unwrap();
    fetch(&url, &part, 1000).unwrap();
    assert_eq!(fs::read(&part).unwrap(), body);
    let requests = served.join().unwrap();
    assert!(requests[0]
        .to_ascii_lowercase()
        .contains("range: bytes=10-"));
    assert!(!requests[1].to_ascii_lowercase().contains("\r\nrange:"));
}

#[test]
fn wrong_payload_switches_mirror_and_preserves_existing_file_on_failure() {
    let dir = workspace();
    let dest = dir.path().join("model.bin");
    let old = payload();
    fs::write(&dest, &old).unwrap();
    let mut html = vec![b' '; 1000];
    html[..6].copy_from_slice(b"<html>");
    let (bad, bad_server) = server(vec![response("200 OK", "Content-Length: 1000\r\n", &html)]);
    assert!(download_one(
        &item(&dest, vec![bad], false),
        &Arc::new(AtomicBool::new(false)),
        None
    )
    .is_err());
    assert_eq!(fs::read(&dest).unwrap(), old);
    bad_server.join().unwrap();
    let (bad, bad_server) = server(vec![response(
        "200 OK",
        "Content-Type: text/html\r\nContent-Length: 1000\r\n",
        &html,
    )]);
    let mut good = old.clone();
    good[10] = 42;
    let (url, served) = server(vec![response("200 OK", "Content-Length: 1000\r\n", &good)]);
    download_one(
        &item(&dest, vec![bad, url], false),
        &Arc::new(AtomicBool::new(false)),
        None,
    )
    .unwrap();
    assert_eq!(fs::read(dest).unwrap(), good);
    bad_server.join().unwrap();
    served.join().unwrap();
}

#[test]
fn cancel_at_transfer_completion_never_publishes_file() {
    let dir = workspace();
    let dest = dir.path().join("model.bin");
    let body = payload();
    let (url, served) = server(vec![response("200 OK", "Content-Length: 1000\r\n", &body)]);
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let progress: ProgressFn = Box::new(move |done, total, _| {
        if done > 0 && done == total {
            flag.store(true, Ordering::SeqCst);
        }
    });
    let err = download_one(&item(&dest, vec![url], false), &cancel, Some(&progress)).unwrap_err();
    assert!(err.to_string().contains("已取消"));
    assert!(!dest.exists());
    served.join().unwrap();
}

#[test]
fn a_bad_existing_model_is_not_marked_present_just_because_of_its_size() {
    let dir = workspace();
    let dest = dir.path().join("model.bin");
    let item = item(&dest, vec![], false);
    fs::write(&dest, vec![0; 1000]).unwrap();
    assert!(!is_present_with_opt(&item, None));
    fs::write(&dest, payload()).unwrap();
    assert!(is_present_with_opt(&item, None));
}

#[test]
fn special_token_vocabulary_is_not_mistaken_for_an_html_error_page() {
    let dir = workspace();
    let path = dir.path().join("tokens.txt");
    fs::write(&path, b"<blk> 0\n<sos/eos> 1\n{ 2\n").unwrap();
    validate_payload(&path, "tokens.txt").unwrap();
    fs::write(&path, b"<!DOCTYPE html><html>error</html>").unwrap();
    assert!(validate_payload(&path, "tokens.txt").is_err());
}

#[test]
fn a_large_onnx_model_still_needs_its_vocabulary_and_is_never_a_custom_executable() {
    let dir = workspace();
    let path = dir.path().join("model.onnx");
    fs::write(&path, vec![8; 2 * 1024 * 1024 + 1]).unwrap();
    let mut item = item(&path, vec![], true);
    item.companion = Some("tokens.txt");
    assert!(!is_present_with_opt(&item, None));
    assert!(!looks_like_custom_build(&item, &path));
    fs::write(dir.path().join("tokens.txt"), b"<unk> 0\n").unwrap();
    assert!(is_present_with_opt(&item, None));
}

#[test]
fn archive_missing_entry_or_companion_never_uses_old_files_as_proof() {
    for missing_entry in [true, false] {
        let dir = workspace();
        let dest = dir.path().join("tool.exe");
        let part = dir.path().join("tool.exe.part");
        fs::write(&dest, b"MZold").unwrap();
        fs::write(dir.path().join("server.exe"), b"MZoldserver").unwrap();
        let bytes = if missing_entry {
            super::tests::tiny_zip(&[("other.exe", b"MZnew")])
        } else {
            super::tests::tiny_zip(&[("tool.exe", b"MZnew")])
        };
        fs::write(&part, bytes).unwrap();
        let mut item = item(&dest, vec![], true);
        item.companion = Some("server.exe");
        assert!(unpack_archive(
            &item,
            &part,
            &dest,
            "https://test.invalid/tool.zip",
            &Arc::new(AtomicBool::new(false))
        )
        .is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"MZold");
        assert_eq!(
            fs::read(dir.path().join("server.exe")).unwrap(),
            b"MZoldserver"
        );
    }
}

#[test]
fn publishing_rolls_back_already_replaced_files_when_a_later_file_fails() {
    let dir = workspace();
    let contents = dir.path().join("contents");
    let dest = dir.path().join("live");
    fs::create_dir_all(contents.join("b-parent")).unwrap();
    fs::create_dir_all(&dest).unwrap();
    fs::write(contents.join("a.exe"), b"MZnew").unwrap();
    fs::write(contents.join("b-parent/b.exe"), b"MZnew").unwrap();
    fs::write(dest.join("a.exe"), b"MZold").unwrap();
    fs::write(dest.join("b-parent"), b"blocks directory creation").unwrap();
    assert!(publish_archive(
        &contents,
        &dir.path().join("backup"),
        &dest,
        &AtomicBool::new(false)
    )
    .is_err());
    assert_eq!(fs::read(dest.join("a.exe")).unwrap(), b"MZold");
    assert_eq!(
        fs::read(dest.join("b-parent")).unwrap(),
        b"blocks directory creation"
    );
}
