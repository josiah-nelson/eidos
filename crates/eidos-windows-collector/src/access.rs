//! Access telemetry aggregation: process classification and per-class
//! counters over file open/read/write/close/delete/rename events. The
//! platform lane feeds events with kernel object identities and a
//! pre-bucketed extension; no path or image name survives past the call.

use eidos_observe::{
    AccessSummary, ExtensionBucket, Histogram, ObjectToken, ProcessClass, StudyKey, TimeAnchor,
};
use lru::LruCache;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    Open,
    Read,
    Write,
    Close,
    Delete,
    Rename,
}

#[derive(Debug, Clone, Copy)]
pub struct AccessEvent {
    pub pid: u32,
    pub kind: AccessKind,
    /// Kernel file identity for the life of the open (FileObject/FileKey);
    /// meaningful only within a trace window.
    pub object: u64,
    pub bytes: u64,
    /// Present on opens; later events inherit it through the object map.
    pub extension: Option<ExtensionBucket>,
}

/// Static classification by image base name (lower-case, without `.exe`).
pub fn classify_image(image: &str) -> Option<ProcessClass> {
    let name = image
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(image)
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    use ProcessClass::*;
    Some(match name {
        "system" | "registry" | "smss" | "csrss" | "wininit" | "services" | "lsass" | "svchost"
        | "winlogon" | "dwm" | "fontdrvhost" | "sihost" | "taskhostw" | "runtimebroker"
        | "dllhost" | "conhost" | "wudfhost" | "spoolsv" | "msiexec" | "tiworker"
        | "trustedinstaller" | "wuauclt" | "usoclient" | "mousocoreworker" | "ctfmon"
        | "audiodg" | "memcompression" | "wmiprvse" | "logonui" | "userinit" | "lsaiso"
        | "secure system" | "vmmem" | "compattelrunner" | "musnotification" | "sppsvc"
        | "wlanext" | "dashost" | "backgroundtaskhost" | "systemsettings" | "wermgr"
        | "werfault" | "ngen" | "ngentask" | "mscorsvw" => System,
        "searchindexer" | "searchprotocolhost" | "searchfilterhost" | "searchhost" | "eidos"
        | "everything" | "es" | "locate32" | "eidos-collector" | "eidos-observe" => Indexer,
        "msmpeng"
        | "mssense"
        | "mpdefendercoreservice"
        | "nissrv"
        | "securityhealthservice"
        | "securityhealthsystray"
        | "smartscreen"
        | "senseir"
        | "sensecncproxy"
        | "sensendr"
        | "csfalconservice"
        | "csfalconcontainer"
        | "sentinelagent"
        | "sentinelservicehost"
        | "cylancesvc"
        | "mcshield"
        | "avp"
        | "ekrn"
        | "bdservicehost" => Security,
        "cargo"
        | "rustc"
        | "cl"
        | "link"
        | "lld"
        | "lld-link"
        | "rc"
        | "mt"
        | "msbuild"
        | "cmake"
        | "ninja"
        | "make"
        | "nmake"
        | "gcc"
        | "g++"
        | "clang"
        | "clang++"
        | "clang-cl"
        | "javac"
        | "csc"
        | "vbcsc"
        | "tsc"
        | "go"
        | "mvn"
        | "gradle"
        | "npm"
        | "yarn"
        | "pnpm"
        | "esbuild"
        | "webpack"
        | "cargo-clippy"
        | "clippy-driver"
        | "rustdoc"
        | "rustfmt"
        | "vctip"
        | "mspdbsrv"
        | "mspdbcmf"
        | "tracker"
        | "vcpkgsrv"
        | "vbcscompiler"
        | "dotnet"
        | "servicehub.roslyncodeanalysisservice"
        | "build-script-build"
        | "wix"
        | "candle"
        | "light"
        | "heat"
        | "signtool"
        | "cargo-metadata"
        | "cargo-nextest"
        | "cargo-fuzz"
        | "cargo-test"
        | "cargo-build" => Build,
        "code"
        | "code-insiders"
        | "code-tunnel"
        | "devenv"
        | "idea64"
        | "pycharm64"
        | "rider64"
        | "clion64"
        | "goland64"
        | "webstorm64"
        | "rust-analyzer"
        | "git"
        | "git-remote-https"
        | "ssh"
        | "ssh-agent"
        | "powershell"
        | "pwsh"
        | "cmd"
        | "windowsterminal"
        | "wt"
        | "openconsole"
        | "bash"
        | "sh"
        | "zsh"
        | "python"
        | "python3"
        | "pythonw"
        | "node"
        | "java"
        | "ruby"
        | "perl"
        | "sqlite3"
        | "claude"
        | "cursor"
        | "notepad++"
        | "sublime_text"
        | "vim"
        | "nvim"
        | "gvim"
        | "emacs"
        | "servicehub.host.dotnet.x64"
        | "servicehub.identityhost"
        | "msvsmon"
        | "vsdebugconsole"
        | "vshost"
        | "codehelper"
        | "gh"
        | "hg"
        | "svn"
        | "tortoiseproc"
        | "tgitcache"
        | "sourcetree"
        | "fork"
        | "gitkraken"
        | "postman"
        | "insomnia"
        | "wireshark" => Development,
        "explorer"
        | "startmenuexperiencehost"
        | "shellexperiencehost"
        | "applicationframehost"
        | "textinputhost"
        | "openwith"
        | "lockapp"
        | "searchapp"
        | "widgets"
        | "phoneexperiencehost"
        | "settings"
        | "quickassist"
        | "snippingtool"
        | "screenclippinghost"
        | "mmc"
        | "taskmgr"
        | "regedit"
        | "control"
        | "rundll32"
        | "shellhost"
        | "dllhost.exe"
        | "smartscreen.exe" => Shell,
        "winword" | "excel" | "powerpnt" | "outlook" | "olk" | "onenote" | "onenoteim"
        | "msteams" | "teams" | "ms-teams" | "slack" | "notepad" | "acrord32" | "acrobat"
        | "sumatrapdf" | "obsidian" | "thunderbird" | "zoom" | "wordpad" | "libreoffice"
        | "soffice" | "soffice.bin" | "evernote" | "notion" | "todo" | "calculator" | "mspaint"
        | "paint" | "visio" | "winproj" | "mspub" | "access" | "lync" => Productivity,
        "chrome" | "msedge" | "firefox" | "brave" | "opera" | "vivaldi" | "msedgewebview2"
        | "iexplore" | "chromium" | "arc" | "waterfox" | "librewolf" | "tor" => Browser,
        "vlc" | "mpc-hc" | "mpc-hc64" | "wmplayer" | "spotify" | "itunes" | "photos"
        | "microsoft.photos" | "obs64" | "ffmpeg" | "handbrake" | "handbrakecli"
        | "plex media server" | "plexmediaserver" | "plextranscoder" | "lightroom"
        | "photoshop" | "premiere" | "afterfx" | "illustrator" | "audacity" | "reaper"
        | "steam" | "steamwebhelper" | "epicgameslauncher" | "discord" | "mpv"
        | "potplayermini64" | "kodi" | "jellyfin" | "musicbee" | "foobar2000" | "gimp"
        | "gimp-2.10" | "inkscape" | "blender" | "davinciresolve" | "resolve" => Media,
        "onedrive" | "filecoauth" | "dropbox" | "dropboxupdate" | "googledrivefs"
        | "googledrive" | "box" | "boxsync" | "megasync" | "syncthing" | "nextcloud" | "icloud"
        | "iclouddrive" | "resilio-sync" | "resilio sync" | "pcloud" | "sync" | "insync"
        | "seafile" | "seadrive" | "mountainduck" | "cyberduck" => CloudSync,
        "veeam.backup.service"
        | "veeam.endpoint.service"
        | "veeam.endpoint.tray"
        | "veeamagent"
        | "reflectbin"
        | "reflectmonitor"
        | "reflectservice"
        | "reflectui"
        | "acronis"
        | "afcdpsrv"
        | "wbengine"
        | "vssvc"
        | "bzserv"
        | "bzfilelist"
        | "bztransmit64"
        | "bzbui"
        | "duplicati.server"
        | "duplicati.gui.trayicon"
        | "restic"
        | "borg"
        | "robocopy"
        | "rclone"
        | "sdclt"
        | "wbadmin"
        | "crashplanservice"
        | "crashplan"
        | "arq"
        | "arqagent"
        | "kopia"
        | "urbackupclientbackend"
        | "xcopy"
        | "syncbackpro"
        | "freefilesync"
        | "goodsync" => Backup,
        "vmwp" | "vmms" | "vmcompute" | "vmconnect" | "vmware-vmx" | "vmware" | "vmware-tray"
        | "vmware-hostd" | "vmnat" | "virtualbox" | "virtualboxvm" | "vboxheadless" | "vboxsvc"
        | "vboxsds" | "docker" | "docker desktop" | "com.docker.backend" | "com.docker.build"
        | "dockerd" | "containerd" | "wsl" | "wslhost" | "wslservice" | "wslrelay"
        | "qemu-system-x86_64" | "hvax64" | "vmmemwsl" | "vmmemcmzygote" | "podman"
        | "multipass" | "multipassd" | "vagrant" | "hyper-v" | "vmsp" => Virtualization,
        _ => return None,
    })
}

/// Class for a process image, falling back to a keyed token of the image
/// base name so unclassified programs remain comparable across hosts
/// sharing a key.
pub fn process_class(image: &str, key: &StudyKey) -> ProcessClass {
    classify_image(image).unwrap_or_else(|| {
        let base = image
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(image)
            .to_ascii_lowercase();
        ProcessClass::ImageToken(key.token("image", base.as_bytes()))
    })
}

#[derive(Default)]
struct ClassStats {
    process_starts: u64,
    opens: u64,
    reads: u64,
    writes: u64,
    closes: u64,
    deletes: u64,
    renames: u64,
    read_bytes: u64,
    write_bytes: u64,
    objects: HashSet<u64>,
    read_objects: HashSet<u64>,
    written_objects: HashSet<u64>,
    read_size: Histogram,
    write_size: Histogram,
    extensions: HashMap<ExtensionBucket, u64>,
}

const MAX_OBJECTS_PER_CLASS: usize = 500_000;

pub struct AccessAggregator {
    pids: HashMap<u32, ProcessClass>,
    /// FileObject -> extension bucket, learned at open.
    objects: LruCache<u64, ExtensionBucket>,
    stats: HashMap<ClassKey, ClassStats>,
    pub unattributed: u64,
}

/// `ProcessClass` carries tokens, so hash on its JSON form.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ClassKey(String);

impl ClassKey {
    fn of(class: &ProcessClass) -> Self {
        Self(serde_json::to_string(class).unwrap_or_default())
    }

    fn class(&self) -> ProcessClass {
        serde_json::from_str(&self.0).unwrap_or(ProcessClass::Other)
    }
}

impl Default for AccessAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl AccessAggregator {
    pub fn new() -> Self {
        Self {
            pids: HashMap::new(),
            objects: LruCache::new(NonZeroUsize::new(262_144).expect("nonzero")),
            stats: HashMap::new(),
            unattributed: 0,
        }
    }

    /// Seed or update the process table; `started` counts a live start
    /// event as opposed to a snapshot of already-running processes.
    pub fn process_seen(&mut self, pid: u32, class: ProcessClass, started: bool) {
        if started {
            self.stats
                .entry(ClassKey::of(&class))
                .or_default()
                .process_starts += 1;
        }
        self.pids.insert(pid, class);
    }

    pub fn process_gone(&mut self, pid: u32) {
        self.pids.remove(&pid);
    }

    pub fn knows_process(&self, pid: u32) -> bool {
        self.pids.contains_key(&pid)
    }

    pub fn observe(&mut self, event: AccessEvent) {
        let class = match self.pids.get(&event.pid) {
            Some(class) => class.clone(),
            None => {
                self.unattributed += 1;
                ProcessClass::Other
            }
        };
        let extension = match event.kind {
            AccessKind::Open => {
                let extension = event.extension.unwrap_or(ExtensionBucket::None);
                self.objects.put(event.object, extension);
                Some(extension)
            }
            _ => self.objects.get(&event.object).copied(),
        };
        let stats = self.stats.entry(ClassKey::of(&class)).or_default();
        if stats.objects.len() < MAX_OBJECTS_PER_CLASS {
            stats.objects.insert(event.object);
        }
        match event.kind {
            AccessKind::Open => {
                stats.opens += 1;
                if let Some(extension) = extension {
                    *stats.extensions.entry(extension).or_default() += 1;
                }
            }
            AccessKind::Read => {
                stats.reads += 1;
                stats.read_bytes += event.bytes;
                stats.read_size.observe(event.bytes);
                if stats.read_objects.len() < MAX_OBJECTS_PER_CLASS {
                    stats.read_objects.insert(event.object);
                }
            }
            AccessKind::Write => {
                stats.writes += 1;
                stats.write_bytes += event.bytes;
                stats.write_size.observe(event.bytes);
                if stats.written_objects.len() < MAX_OBJECTS_PER_CLASS {
                    stats.written_objects.insert(event.object);
                }
            }
            AccessKind::Close => {
                stats.closes += 1;
                self.objects.pop(&event.object);
            }
            AccessKind::Delete => stats.deletes += 1,
            AccessKind::Rename => stats.renames += 1,
        }
    }

    pub fn flush(&mut self, at: TimeAnchor, interval_s: u32) -> Vec<AccessSummary> {
        let mut summaries: Vec<AccessSummary> = self
            .stats
            .drain()
            .map(|(class, stats)| {
                let mut extensions: Vec<(ExtensionBucket, u64)> =
                    stats.extensions.into_iter().collect();
                extensions.sort();
                AccessSummary {
                    at: at.clone(),
                    interval_s,
                    process: class.class(),
                    process_starts: stats.process_starts,
                    opens: stats.opens,
                    reads: stats.reads,
                    writes: stats.writes,
                    closes: stats.closes,
                    deletes: stats.deletes,
                    renames: stats.renames,
                    read_bytes: stats.read_bytes,
                    write_bytes: stats.write_bytes,
                    distinct_objects: stats.objects.len() as u64,
                    read_write_objects: stats
                        .read_objects
                        .intersection(&stats.written_objects)
                        .count() as u64,
                    read_size: stats.read_size,
                    write_size: stats.write_size,
                    extensions,
                }
            })
            .collect();
        summaries.sort_by(|a, b| b.opens.cmp(&a.opens).then(b.reads.cmp(&a.reads)));
        summaries
    }
}

/// Convenience for the lane's fallback image lookups.
pub fn image_token(image: &str, key: &StudyKey) -> ObjectToken {
    key.token("image", image.to_ascii_lowercase().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_classify_by_base_name_and_fall_back_to_tokens() {
        assert_eq!(
            classify_image(r"C:\Program Files\Rust\cargo.EXE"),
            Some(ProcessClass::Build)
        );
        assert_eq!(classify_image("MsMpEng.exe"), Some(ProcessClass::Security));
        assert_eq!(
            classify_image(r"\Device\HarddiskVolume3\Windows\explorer.exe"),
            Some(ProcessClass::Shell)
        );
        assert_eq!(
            classify_image("OneDrive.exe"),
            Some(ProcessClass::CloudSync)
        );
        assert_eq!(classify_image("invented-tool.exe"), None);
        let key = StudyKey::from_bytes([1; 32]);
        let class = process_class(r"D:\tools\Invented-Tool.exe", &key);
        assert_eq!(
            class,
            ProcessClass::ImageToken(image_token("invented-tool.exe", &key))
        );
        assert!(!serde_json::to_string(&class).unwrap().contains("invented"));
    }

    #[test]
    fn aggregates_per_class_with_object_inheritance() {
        let mut aggregator = AccessAggregator::new();
        aggregator.process_seen(10, ProcessClass::Build, true);
        aggregator.process_seen(11, ProcessClass::Browser, false);
        let open = |pid, object, extension| AccessEvent {
            pid,
            kind: AccessKind::Open,
            object,
            bytes: 0,
            extension: Some(extension),
        };
        let io = |pid, kind, object, bytes| AccessEvent {
            pid,
            kind,
            object,
            bytes,
            extension: None,
        };
        aggregator.observe(open(10, 1, ExtensionBucket::Source));
        aggregator.observe(io(10, AccessKind::Read, 1, 4096));
        aggregator.observe(io(10, AccessKind::Write, 1, 100));
        aggregator.observe(io(10, AccessKind::Close, 1, 0));
        aggregator.observe(open(10, 2, ExtensionBucket::Build));
        aggregator.observe(io(10, AccessKind::Write, 2, 65536));
        aggregator.observe(io(11, AccessKind::Read, 3, 10));
        aggregator.observe(io(99, AccessKind::Read, 4, 10));
        let at = TimeAnchor {
            monotonic_ns: 1,
            utc_ns: 2,
        };
        let summaries = aggregator.flush(at, 60);
        assert_eq!(summaries.len(), 3);
        let build = summaries
            .iter()
            .find(|s| s.process == ProcessClass::Build)
            .unwrap();
        assert_eq!(build.process_starts, 1);
        assert_eq!(build.opens, 2);
        assert_eq!(build.reads, 1);
        assert_eq!(build.writes, 2);
        assert_eq!(build.closes, 1);
        assert_eq!(build.read_bytes, 4096);
        assert_eq!(build.write_bytes, 65636);
        assert_eq!(build.distinct_objects, 2);
        assert_eq!(build.read_write_objects, 1);
        assert_eq!(
            build.extensions,
            vec![(ExtensionBucket::Source, 1), (ExtensionBucket::Build, 1)]
        );
        let other = summaries
            .iter()
            .find(|s| s.process == ProcessClass::Other)
            .unwrap();
        assert_eq!(other.reads, 1);
        assert_eq!(aggregator.unattributed, 1);
        assert!(aggregator
            .flush(
                TimeAnchor {
                    monotonic_ns: 3,
                    utc_ns: 4
                },
                60
            )
            .is_empty());
    }
}
