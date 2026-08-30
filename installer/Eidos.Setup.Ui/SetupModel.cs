using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.IO;
using System.Runtime.CompilerServices;
using System.Security;
using System.Threading.Tasks;
using System.Windows.Input;
using System.Windows.Threading;
using Microsoft.Win32;
using WixToolset.BootstrapperApplicationApi;

namespace Eidos.Setup
{
    public enum Page
    {
        Loading,
        Welcome,
        Scope,
        Location,
        Account,
        Options,
        Progress,
        Success,
        Failure,
        Maintenance,
        Remove,
        Blocked,
    }

    public enum SetupState
    {
        Detecting,
        Ready,
        Planning,
        Applying,
        Applied,
        Failed,
    }

    public enum AccountKind
    {
        LocalSystem,
        LocalService,
        NetworkService,
        User,
    }

    /// <summary>
    /// Everything the setup window shows and every choice it collects. The
    /// bootstrapper feeds engine events in; the window binds to the rest.
    /// </summary>
    public sealed class SetupModel : INotifyPropertyChanged
    {
        private const int ErrorProductVersion = unchecked((int)0x80070666);
        private const int ErrorServiceNotActive = unchecked((int)0x80070426);
        private const int ErrorLogonFailure = unchecked((int)0x8007052E);
        private const int ErrorInvalidArgument = unchecked((int)0x80070057);
        private const string RegistryKey = @"Software\eidos";
        private const string CollectorRegistryKey = @"Software\eidos-collector";
        private const string CollectorBundleUpgradeCode = "5D2B93F8-29ED-4E6C-B101-1355B4A36F3A";
        private const string UninstallRegistryKey = @"Software\Microsoft\Windows\CurrentVersion\Uninstall\";

        private readonly EidosBootstrapper ba;
        private readonly HashSet<string> collectorRelatedBundles = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        private Page page = Page.Loading;
        private SetupState state = SetupState.Detecting;
        private bool installed;
        private bool rememberedCore;
        private string rememberedCoreVersion;
        private bool newerInstalled;
        private string olderVersion;
        private bool detectedPerMachine;
        private bool perMachine;
        private bool installDirEdited;
        private bool dataDirEdited;
        private string installDir = "";
        private string dataDir = "";
        private string bind = "127.0.0.1";
        private string port = "7700";
        private AccountKind account = AccountKind.LocalSystem;
        private string accountUser;
        private SecureString password;
        private string accountStatus;
        private bool accountVerified;
        private bool startService = true;
        private bool startMenu = true;
        private bool launchAfter = true;
        private bool removeData;
        private bool installCollector;
        private bool collectorInstalled;
        private bool removeCollector = true;
        private bool removeCollectorData;
        private bool repair = true;
        private string validation;
        private int progress;
        private string progressMessage = "";
        private string errorMessage;
        private bool canceled;
        private bool restartRequired;
        private int? collectorError;
        private string dataSize;
        private LaunchAction plannedAction;

        public SetupModel(EidosBootstrapper ba)
        {
            this.ba = ba;
            this.Version = ba.Engine.GetVariableVersion("WixBundleVersion");
            this.accountUser = Native.CurrentUser();
            this.perMachine = Native.IsAdministrator();
            this.ApplyScopeDefaults();
            this.ReadRememberedSettings();
            this.ReadCollectorState();

            ba.DetectBegin += this.OnDetectBegin;
            ba.DetectRelatedBundle += this.OnDetectRelatedBundle;
            ba.DetectCompatibleMsiPackage += this.OnDetectCompatibleMsiPackage;
            ba.DetectPackageComplete += this.OnDetectPackageComplete;
            ba.DetectComplete += this.OnDetectComplete;
            ba.PlanPackageBegin += this.OnPlanPackageBegin;
            ba.PlanCompatibleMsiPackageBegin += this.OnPlanCompatibleMsiPackageBegin;
            ba.PlanRelatedBundleType += this.OnPlanRelatedBundleType;
            ba.PlanComplete += this.OnPlanComplete;
            ba.ApplyBegin += this.OnApplyBegin;
            ba.Progress += this.OnProgress;
            ba.CacheAcquireProgress += this.OnCacheProgress;
            ba.ExecuteProgress += this.OnExecuteProgress;
            ba.ExecutePackageBegin += this.OnExecutePackageBegin;
            ba.ExecutePackageComplete += this.OnExecutePackageComplete;
            ba.ExecuteMsiMessage += this.OnExecuteMsiMessage;
            ba.Error += this.OnError;
            ba.ApplyComplete += this.OnApplyComplete;

            this.NextCommand = new RelayCommand(_ => this.Next(), _ => this.CanGoNext);
            this.BackCommand = new RelayCommand(_ => this.Back(), _ => this.CanGoBack);
            this.CancelCommand = new RelayCommand(_ => this.Cancel());
            this.CloseCommand = new RelayCommand(_ => EidosBootstrapper.View?.Close());
            this.BrowseInstallDirCommand = new RelayCommand(_ => this.Browse(true));
            this.BrowseDataDirCommand = new RelayCommand(_ => this.Browse(false));
            this.VerifyAccountCommand = new RelayCommand(_ => this.VerifyAccount(), _ => this.Account == AccountKind.User);
            this.OpenLogCommand = new RelayCommand(_ => Native.ShellOpen(this.LogPath));
            this.OpenEidosCommand = new RelayCommand(_ => Native.ShellOpen(this.LaunchUrl));
            this.OpenLicenseCommand = new RelayCommand(_ => Native.ShellOpen("https://www.gnu.org/licenses/agpl-3.0.html"));
        }

        public event PropertyChangedEventHandler PropertyChanged;

        public IEngine Engine => this.ba.Engine;
        public Dispatcher Dispatcher { get; set; }
        public IntPtr WindowHandle { get; set; }
        public int ExitCode { get; private set; }
        public string Version { get; }
        public string Title => "eidos Setup";
        public string LogPath => this.Engine.GetVariableString("WixBundleLog");

        public ICommand NextCommand { get; }
        public ICommand BackCommand { get; }
        public ICommand CancelCommand { get; }
        public ICommand CloseCommand { get; }
        public ICommand BrowseInstallDirCommand { get; }
        public ICommand BrowseDataDirCommand { get; }
        public ICommand VerifyAccountCommand { get; }
        public ICommand OpenLogCommand { get; }
        public ICommand OpenEidosCommand { get; }
        public ICommand OpenLicenseCommand { get; }

        // ----- state ----------------------------------------------------------

        public Page Page
        {
            get => this.page;
            set
            {
                if (this.Set(ref this.page, value))
                {
                    this.Raise(nameof(this.PageTitle), nameof(this.PageSubtitle), nameof(this.PrimaryLabel),
                        nameof(this.CanGoBack), nameof(this.CanGoNext), nameof(this.ShowPrimary), nameof(this.ShowBack),
                        nameof(this.SecondaryLabel), nameof(this.IsBusy));
                }
            }
        }

        public SetupState State
        {
            get => this.state;
            set
            {
                if (this.Set(ref this.state, value))
                {
                    this.Raise(nameof(this.CanGoNext), nameof(this.IsBusy), nameof(this.SecondaryLabel));
                }
            }
        }

        public bool IsBusy => this.State == SetupState.Planning || this.State == SetupState.Applying;

        public string PageTitle
        {
            get
            {
                switch (this.Page)
                {
                    case Page.Loading: return "Checking this computer";
                    case Page.Welcome: return this.olderVersion != null ? $"Upgrade eidos {this.olderVersion} to {this.Version}" : $"Welcome to eidos {this.Version}";
                    case Page.Scope: return "Who is eidos for?";
                    case Page.Location: return "Where should eidos live?";
                    case Page.Account: return "Which account runs the service?";
                    case Page.Options: return "Ready to install";
                    case Page.Progress: return this.plannedAction == LaunchAction.Uninstall ? "Removing eidos" : this.plannedAction == LaunchAction.Repair ? "Repairing eidos" : "Installing eidos";
                    case Page.Success: return this.plannedAction == LaunchAction.Uninstall ? "eidos has been removed" : "eidos is ready";
                    case Page.Failure: return this.canceled ? "Setup was cancelled" : "Setup did not finish";
                    case Page.Maintenance: return $"eidos {this.Version} is installed";
                    case Page.Remove: return "Remove eidos";
                    case Page.Blocked: return "A newer eidos is already installed";
                    default: return "";
                }
            }
        }

        public string PageSubtitle
        {
            get
            {
                switch (this.Page)
                {
                    case Page.Welcome: return this.olderVersion != null
                        ? "Your settings, sources and indexed data are kept. The service is restarted on the new version."
                        : "Search every file on this computer and its shares by name, path, size, date, and content.";
                    case Page.Scope: return "This choice decides where files go and whether eidos runs as a Windows service.";
                    case Page.Location: return "The data folder holds the catalog and search indexes; it can grow to a few percent of the indexed data.";
                    case Page.Account: return "The service only sees what its account can see. Network shares need a real user.";
                    case Page.Options: return "Review the choices below, then install.";
                    case Page.Progress: return "This usually takes less than a minute.";
                    case Page.Maintenance: return "Repair the installation or remove eidos from this computer.";
                    case Page.Remove: return "The program files, service and shortcuts are removed. Your indexed data stays unless you say otherwise.";
                    default: return "";
                }
            }
        }

        public string PrimaryLabel
        {
            get
            {
                switch (this.Page)
                {
                    case Page.Welcome: return this.olderVersion != null
                        ? this.PerMachine && this.Account == AccountKind.User
                            ? "Next"
                            : this.NeedsElevation && !Native.IsAdministrator() ? "Upgrade (administrator approval)" : "Upgrade"
                        : "Next";
                    case Page.Account: return this.olderVersion != null
                        ? this.NeedsElevation && !Native.IsAdministrator() ? "Upgrade (administrator approval)" : "Upgrade"
                        : "Next";
                    case Page.Options: return this.NeedsElevation && !Native.IsAdministrator() ? "Install (administrator approval)" : "Install";
                    case Page.Maintenance: return this.Repair
                        ? this.NeedsElevation && !Native.IsAdministrator() ? "Repair (administrator approval)" : "Repair"
                        : "Next";
                    case Page.Remove: return "Remove";
                    case Page.Success: return this.plannedAction != LaunchAction.Uninstall && this.launchAfter ? "Open eidos" : "Close";
                    case Page.Failure: return "Close";
                    case Page.Blocked: return "Close";
                    default: return "Next";
                }
            }
        }

        public string SecondaryLabel => this.Page == Page.Success || this.Page == Page.Failure || this.Page == Page.Blocked ? "" : "Cancel";
        public bool ShowPrimary => this.Page != Page.Loading && this.Page != Page.Progress;
        public bool ShowBack => this.CanGoBack;

        public bool CanGoBack
        {
            get
            {
                switch (this.Page)
                {
                    case Page.Scope:
                    case Page.Location:
                    case Page.Account:
                    case Page.Options:
                    case Page.Remove:
                        return !this.IsBusy;
                    default:
                        return false;
                }
            }
        }

        public bool CanGoNext => this.ShowPrimary && !this.IsBusy;

        // ----- detection -------------------------------------------------------

        public bool Installed
        {
            get => this.installed;
            private set => this.Set(ref this.installed, value);
        }

        public string DetectedScopeText => this.detectedPerMachine ? "for all users (Windows service)" : "for this user";
        public bool IsUpgrade => this.olderVersion != null;

        // ----- choices ----------------------------------------------------------

        public bool PerMachine
        {
            get => this.perMachine;
            set
            {
                if (this.Set(ref this.perMachine, value))
                {
                    this.Raise(nameof(this.PerUser), nameof(this.PrimaryLabel), nameof(this.ElevationNote), nameof(this.SummaryText), nameof(this.StartLabel), nameof(this.CollectorScopeNote));
                    this.ApplyScopeDefaults();
                }
            }
        }

        public bool PerUser
        {
            get => !this.perMachine;
            set => this.PerMachine = !value;
        }

        private bool NeedsElevation => this.PerMachine || this.InstallCollector;

        public string ElevationNote => !this.NeedsElevation || Native.IsAdministrator()
            ? ""
            : this.PerMachine
                ? "Windows will ask for administrator approval when the installation starts."
                : "eidos stays installed just for you, but the collector is a system service, so Windows will ask for administrator approval.";

        public string InstallDir
        {
            get => this.installDir;
            set
            {
                if (this.Set(ref this.installDir, value))
                {
                    this.installDirEdited = true;
                    this.Validation = null;
                    this.Raise(nameof(this.SummaryText));
                }
            }
        }

        public string DataDir
        {
            get => this.dataDir;
            set
            {
                if (this.Set(ref this.dataDir, value))
                {
                    this.dataDirEdited = true;
                    this.Validation = null;
                    this.Raise(nameof(this.SummaryText), nameof(this.RemoveDataLabel));
                }
            }
        }

        public string Bind
        {
            get => this.bind;
            set
            {
                if (this.Set(ref this.bind, (value ?? "").Trim()))
                {
                    this.Validation = null;
                    this.Raise(nameof(this.BindWarning), nameof(this.Url),nameof(this.SummaryText));
                }
            }
        }

        public string[] BindChoices { get; } = { "127.0.0.1", "0.0.0.0" };

        public string BindWarning => this.bind == "127.0.0.1" || this.bind == "::1" || this.bind == "localhost"
            ? ""
            : "eidos has no login yet. On this address anyone who can reach this computer can search and read everything it indexes. Keep 127.0.0.1 unless the network is trusted.";

        public string Port
        {
            get => this.port;
            set
            {
                if (this.Set(ref this.port, (value ?? "").Trim()))
                {
                    this.Validation = null;
                    this.Raise(nameof(this.Url),nameof(this.SummaryText));
                }
            }
        }

        /// <summary>The address as chosen, shown verbatim.</summary>
        public string Url => $"http://{(string.IsNullOrEmpty(this.bind) ? "127.0.0.1" : this.bind)}:{this.port}/";

        /// <summary>What a browser on this computer opens: a wildcard bind is reached on loopback.</summary>
        public string LaunchUrl
        {
            get
            {
                var host = this.bind == "0.0.0.0" || this.bind == "::" || string.IsNullOrEmpty(this.bind) ? "127.0.0.1" : this.bind;
                return $"http://{host}:{this.port}/";
            }
        }

        public AccountKind Account
        {
            get => this.account;
            set
            {
                if (this.Set(ref this.account, value))
                {
                    this.AccountStatus = null;
                    this.accountVerified = false;
                    this.Raise(nameof(this.IsLocalSystem), nameof(this.IsLocalService), nameof(this.IsNetworkService),
                        nameof(this.IsUserAccount), nameof(this.SummaryText), nameof(this.AccountExplanation));
                }
            }
        }

        public bool IsLocalSystem { get => this.account == AccountKind.LocalSystem; set { if (value) this.Account = AccountKind.LocalSystem; } }
        public bool IsLocalService { get => this.account == AccountKind.LocalService; set { if (value) this.Account = AccountKind.LocalService; } }
        public bool IsNetworkService { get => this.account == AccountKind.NetworkService; set { if (value) this.Account = AccountKind.NetworkService; } }
        public bool IsUserAccount { get => this.account == AccountKind.User; set { if (value) this.Account = AccountKind.User; } }

        public string AccountExplanation
        {
            get
            {
                switch (this.account)
                {
                    case AccountKind.LocalSystem:
                        return "Full access to every local drive. No network identity: mapped drives and \\\\server\\share paths are not visible, so only local disks can be indexed.";
                    case AccountKind.User:
                        return "Runs as you (or another user). The service sees exactly what that account can open, including network shares it has access to. The password is stored by Windows for the service, never by eidos.";
                    default:
                        return "";
                }
            }
        }

        public string AccountUser
        {
            get => this.accountUser;
            set
            {
                if (this.Set(ref this.accountUser, (value ?? "").Trim()))
                {
                    this.AccountStatus = null;
                    this.accountVerified = false;
                    this.Raise(nameof(this.SummaryText));
                }
            }
        }

        public SecureString Password
        {
            get => this.password;
            set
            {
                this.password = value;
                this.AccountStatus = null;
                this.accountVerified = false;
            }
        }

        public string AccountStatus
        {
            get => this.accountStatus;
            private set => this.Set(ref this.accountStatus, value);
        }

        public bool StartService { get => this.startService; set { if (this.Set(ref this.startService, value)) this.Raise(nameof(this.SummaryText)); } }
        public string StartLabel => this.PerMachine
            ? "Start the service now and whenever Windows starts"
            : "Start eidos now and whenever you sign in";
        public bool StartMenu { get => this.startMenu; set { if (this.Set(ref this.startMenu, value)) this.Raise(nameof(this.SummaryText)); } }
        public bool LaunchAfter { get => this.launchAfter; set { if (this.Set(ref this.launchAfter, value)) this.Raise(nameof(this.PrimaryLabel)); } }
        public bool RemoveData { get => this.removeData; set => this.Set(ref this.removeData, value); }

        /// <summary>
        /// The profiling collector: a separate LocalSystem service with its own
        /// data directory. It can accompany either core scope and therefore can
        /// be the only package that elevates. The checkbox reflects the detected
        /// state on maintenance and upgrade, so leaving it alone never removes
        /// a collector that is there.
        /// </summary>
        public bool InstallCollector
        {
            get => this.installCollector;
            set { if (this.Set(ref this.installCollector, value)) this.Raise(nameof(this.SummaryText), nameof(this.ElevationNote), nameof(this.PrimaryLabel), nameof(this.CollectorScopeNote), nameof(this.SuccessText)); }
        }
        public bool CollectorInstalled { get => this.collectorInstalled; private set { if (this.Set(ref this.collectorInstalled, value)) this.Raise(nameof(this.CollectorLabel), nameof(this.RemoveCollectorLabel)); } }
        public bool CanChooseCollector => true;
        public string CollectorLabel => this.collectorInstalled ? "Keep the profiling collector installed" : "Install profiling collector";
        public string CollectorHint => "Runs a separate privileged service alongside eidos and records bounded, privacy-preserving workload measurements. Its data directory, identity and removal are independent of eidos.";
        public string CollectorScopeNote => this.PerMachine
            ? ""
            : this.InstallCollector
                ? "The collector installs for the computer; the core remains just for you."
                : "The collector can be added to a just-for-you install, but its system service requires administrator approval.";
        public bool RemoveCollector { get => this.removeCollector; set { if (this.Set(ref this.removeCollector, value)) this.Raise(nameof(this.SuccessText)); } }
        public bool RemoveCollectorData { get => this.removeCollectorData; set => this.Set(ref this.removeCollectorData, value); }
        public string RemoveCollectorLabel => "Also remove the profiling collector service";
        public string RemoveCollectorDataLabel => "Also delete the collector's study data (spool, configuration, study key)";
        public bool Repair { get => this.repair; set { if (this.Set(ref this.repair, value)) this.Raise(nameof(this.Uninstall), nameof(this.PrimaryLabel)); } }
        public bool Uninstall { get => !this.repair; set => this.Repair = !value; }

        public string RemoveDataLabel => $"Also delete the indexed data in {this.dataDir}{(this.dataSize != null ? " (" + this.dataSize + ")" : "")}";

        public string Validation
        {
            get => this.validation;
            private set => this.Set(ref this.validation, value);
        }

        public string SummaryText
        {
            get
            {
                var scope = this.PerMachine ? "All users, as a Windows service" : "Just you, no service";
                var lines = $"Install for:\t{scope}\nProgram files:\t{this.installDir}\nData folder:\t{this.dataDir}\nWeb address:\t{this.Url}";
                if (this.PerMachine)
                {
                    var acct = this.account == AccountKind.LocalSystem ? "LocalSystem"
                        : this.account == AccountKind.LocalService ? "Local Service"
                        : this.account == AccountKind.NetworkService ? "Network Service"
                        : this.accountUser;
                    lines += $"\nService account:\t{acct}\nStart service:\t{(this.startService ? "now and at every boot" : "later, by hand")}";
                }
                else
                {
                    lines += $"\nStart eidos:\t{(this.startService ? "now and at every sign-in" : "from the Start menu")}";
                }
                lines += $"\nStart menu:\t{(this.startMenu ? "eidos shortcuts" : "none")}";
                lines += $"\nCollector:\t{(this.installCollector ? (this.collectorInstalled ? "kept (separate service)" : "installed as a separate service") : (this.collectorInstalled ? "left as installed" : "not installed"))}";
                return lines;
            }
        }

        // ----- progress / result -----------------------------------------------

        public int Progress { get => this.progress; private set => this.Set(ref this.progress, value); }
        public string ProgressMessage { get => this.progressMessage; private set => this.Set(ref this.progressMessage, value); }
        public string ErrorMessage { get => this.errorMessage; private set => this.Set(ref this.errorMessage, value); }
        public bool RestartRequired { get => this.restartRequired; private set => this.Set(ref this.restartRequired, value); }
        public string SuccessText
        {
            get
            {
                if (this.plannedAction == LaunchAction.Uninstall)
                {
                    return (this.removeData ? "The program and its data were removed." : $"The program was removed. Your indexed data is still in {this.dataDir}; delete that folder if you no longer want it.")
                        + (this.collectorInstalled ? (this.removeCollector ? (this.removeCollectorData ? "\nThe profiling collector and its study data were removed." : "\nThe profiling collector service was removed; its study data was kept.") : "\nThe profiling collector service was kept.") : "");
                }
                var result = this.PerMachine
                    ? $"eidos is running at {this.Url}.\nThe service starts with Windows."
                    : this.startService
                        ? $"eidos is running at {this.Url}.\nIt runs in the background and starts again when you sign in."
                        : $"eidos is installed. \"Start eidos\" in the Start menu runs it in the background at {this.Url}.";
                return result + (this.installCollector ? "\nThe profiling collector runs as the eidos-collector service." : "");
            }
        }

        public bool Canceled
        {
            get => this.canceled;
            private set => this.Set(ref this.canceled, value);
        }

        // ----- navigation -------------------------------------------------------

        private void Next()
        {
            switch (this.Page)
            {
                case Page.Welcome:
                    if (this.olderVersion != null)
                    {
                        // Upgrade: the MSI reads the remembered core settings.
                        // The collector checkbox is the one explicit package
                        // choice that can change independently.
                        if (this.PerMachine && this.Account == AccountKind.User)
                        {
                            // Windows does not expose a service account's
                            // password. Ask again before the major upgrade
                            // removes and recreates the service registration.
                            this.Page = Page.Account;
                        }
                        else
                        {
                            this.StartPlan(LaunchAction.Install);
                        }
                    }
                    else
                    {
                        this.Page = Page.Scope;
                    }
                    break;
                case Page.Scope:
                    this.Page = Page.Location;
                    break;
                case Page.Location:
                    if (this.ValidateLocation())
                    {
                        this.Page = this.PerMachine ? Page.Account : Page.Options;
                    }
                    break;
                case Page.Account:
                    if (this.ValidateAccount())
                    {
                        if (this.olderVersion != null)
                        {
                            this.StartPlan(LaunchAction.Install);
                        }
                        else
                        {
                            this.Page = Page.Options;
                        }
                    }
                    break;
                case Page.Options:
                    this.StartPlan(LaunchAction.Install);
                    break;
                case Page.Maintenance:
                    if (this.Repair)
                    {
                        this.StartPlan(LaunchAction.Repair);
                    }
                    else
                    {
                        this.Page = Page.Remove;
                    }
                    break;
                case Page.Remove:
                    this.StartPlan(LaunchAction.Uninstall);
                    break;
                case Page.Success:
                    if (this.plannedAction != LaunchAction.Uninstall && this.launchAfter)
                    {
                        Native.ShellOpen(this.LaunchUrl);
                    }
                    EidosBootstrapper.View?.Close();
                    break;
                case Page.Failure:
                case Page.Blocked:
                    EidosBootstrapper.View?.Close();
                    break;
            }
        }

        private void Back()
        {
            switch (this.Page)
            {
                case Page.Scope: this.Page = Page.Welcome; break;
                case Page.Location: this.Page = Page.Scope; break;
                case Page.Account: this.Page = this.olderVersion != null ? Page.Welcome : Page.Location; break;
                case Page.Options: this.Page = this.PerMachine ? Page.Account : Page.Location; break;
                case Page.Remove: this.Page = Page.Maintenance; break;
            }
        }

        /// <summary>Cancel button and window close: confirm during apply.</summary>
        public void Cancel()
        {
            if (this.State == SetupState.Applying)
            {
                if (this.Canceled)
                {
                    return;
                }
                var answer = System.Windows.MessageBox.Show(EidosBootstrapper.View, "Stop the installation? Changes made so far are rolled back.", "eidos Setup",
                    System.Windows.MessageBoxButton.YesNo, System.Windows.MessageBoxImage.Question);
                this.Canceled = answer == System.Windows.MessageBoxResult.Yes;
                return;
            }
            EidosBootstrapper.View?.Close();
        }

        private void Browse(bool install)
        {
            var current = install ? this.installDir : this.dataDir;
            var picked = FolderPicker.Pick(this.WindowHandle, install ? "Choose the program folder" : "Choose the data folder", Directory.Exists(current) ? current : Path.GetDirectoryName(current));
            if (picked == null)
            {
                return;
            }
            // Picking an existing parent means "put eidos inside it".
            if (!string.Equals(Path.GetFileName(picked), "eidos", StringComparison.OrdinalIgnoreCase) && Directory.Exists(picked) && Directory.EnumerateFileSystemEntries(picked).GetEnumerator().MoveNext())
            {
                picked = Path.Combine(picked, "eidos");
            }
            if (install)
            {
                this.InstallDir = picked;
            }
            else
            {
                this.DataDir = picked;
            }
        }

        private void ApplyScopeDefaults()
        {
            if (!this.installDirEdited)
            {
                this.installDir = this.perMachine
                    ? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), "eidos")
                    : Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Programs", "eidos");
                this.Raise(nameof(this.InstallDir));
            }
            if (!this.dataDirEdited)
            {
                this.dataDir = this.perMachine
                    ? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.CommonApplicationData), "eidos")
                    : Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "eidos");
                this.Raise(nameof(this.DataDir), nameof(this.RemoveDataLabel));
            }
        }

        /// <summary>Prefill from a previous install so repair/upgrade/remove show real paths.</summary>
        private void ReadRememberedSettings()
        {
            foreach (var root in new[] { Registry.LocalMachine, Registry.CurrentUser })
            {
                try
                {
                    using (var key = root.OpenSubKey(RegistryKey))
                    {
                        if (key == null)
                        {
                            continue;
                        }
                        var data = key.GetValue("DataDir") as string;
                        if (!string.IsNullOrEmpty(data))
                        {
                            this.dataDir = data.TrimEnd('\\');
                            this.dataDirEdited = true;
                        }
                        var install = key.GetValue("InstallDir") as string;
                        if (!string.IsNullOrEmpty(install))
                        {
                            this.installDir = install.TrimEnd('\\');
                            this.installDirEdited = true;
                        }
                        var port = key.GetValue("Port") as string;
                        if (!string.IsNullOrEmpty(port))
                        {
                            this.port = port;
                        }
                        var bind = key.GetValue("Bind") as string;
                        if (!string.IsNullOrEmpty(bind))
                        {
                            this.bind = bind;
                        }
                        this.startMenu = key.GetValue("StartMenu") != null;
                        if (root == Registry.LocalMachine)
                        {
                            this.startService = key.GetValue("StartService") != null;
                            var kind = key.GetValue("ServiceAccountKind") as string;
                            this.account = string.Equals(kind, "local-service", StringComparison.OrdinalIgnoreCase) ? AccountKind.LocalService
                                : string.Equals(kind, "network-service", StringComparison.OrdinalIgnoreCase) ? AccountKind.NetworkService
                                : string.Equals(kind, "user", StringComparison.OrdinalIgnoreCase) ? AccountKind.User
                                : AccountKind.LocalSystem;
                            if (this.account == AccountKind.User)
                            {
                                var domain = key.GetValue("ServiceDomain") as string;
                                var user = key.GetValue("ServiceUser") as string;
                                if (!string.IsNullOrEmpty(user))
                                {
                                    this.accountUser = string.IsNullOrEmpty(domain) || domain == "." ? user : domain + "\\" + user;
                                }
                            }
                        }
                        else
                        {
                            using (var run = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run"))
                            {
                                this.startService = run?.GetValue("eidos") != null;
                            }
                        }
                        this.detectedPerMachine = root == Registry.LocalMachine;
                        this.rememberedCore = true;
                        this.rememberedCoreVersion = key.GetValue("Version") as string;
                        if (!string.IsNullOrEmpty(this.rememberedCoreVersion))
                        {
                            if (this.Engine.CompareVersions(this.Version, this.rememberedCoreVersion) >= 0)
                            {
                                this.olderVersion = this.rememberedCoreVersion;
                            }
                            else
                            {
                                this.newerInstalled = true;
                            }
                        }
                        return;
                    }
                }
                catch
                {
                    // unreadable hive: fall back to defaults
                }
            }
        }

        /// <summary>
        /// Burn detects the exact collector ProductCode. A previous major
        /// version has a different ProductCode, so use the MSI-owned registry
        /// value to keep that installed package selected during adoption.
        /// </summary>
        private void ReadCollectorState()
        {
            try
            {
                using (var key = Registry.LocalMachine.OpenSubKey(CollectorRegistryKey))
                {
                    this.collectorInstalled = key?.GetValue("Version") is string;
                    this.installCollector = this.collectorInstalled;
                }
            }
            catch
            {
                // Detection can still recognize the exact or a newer package.
            }
        }

        private bool ValidateLocation()
        {
            string Check(string path, string what)
            {
                if (string.IsNullOrWhiteSpace(path))
                {
                    return $"Choose a {what}.";
                }
                if (!Path.IsPathRooted(path) || path.IndexOfAny(Path.GetInvalidPathChars()) >= 0)
                {
                    return $"The {what} must be a full path such as C:\\eidos.";
                }
                var root = Path.GetPathRoot(path);
                if (!Directory.Exists(root))
                {
                    return $"The drive for the {what} ({root}) is not available.";
                }
                return null;
            }

            var problem = Check(this.installDir, "program folder") ?? Check(this.dataDir, "data folder");
            if (problem == null && string.Equals(Path.GetFullPath(this.installDir).TrimEnd('\\'), Path.GetFullPath(this.dataDir).TrimEnd('\\'), StringComparison.OrdinalIgnoreCase))
            {
                problem = "Use different folders for the program and the data.";
            }
            if (problem == null)
            {
                if (!int.TryParse(this.port, out var p) || p < 1 || p > 65535)
                {
                    problem = "The port must be a number between 1 and 65535.";
                }
                else if (!Native.IsPortFree(this.bind, p))
                {
                    problem = $"Port {p} is already in use on {this.bind}. Choose another port.";
                }
            }
            if (problem == null && !System.Net.IPAddress.TryParse(this.bind, out _))
            {
                problem = "The listen address must be an IP address, such as 127.0.0.1.";
            }
            this.Validation = problem;
            return problem == null;
        }

        private bool ValidateAccount()
        {
            if (this.account != AccountKind.User)
            {
                return true;
            }
            if (!this.accountVerified)
            {
                this.VerifyAccount();
            }
            return this.accountVerified;
        }

        private void VerifyAccount()
        {
            var name = this.accountUser ?? "";
            string domain, user;
            var slash = name.IndexOf('\\');
            var at = name.IndexOf('@');
            if (slash > 0)
            {
                domain = name.Substring(0, slash);
                user = name.Substring(slash + 1);
            }
            else if (at > 0)
            {
                user = name.Substring(0, at);
                domain = name.Substring(at + 1);
            }
            else
            {
                domain = ".";
                user = name;
            }
            var problem = Native.ValidateCredentials(domain, user, this.password);
            this.accountVerified = problem == null;
            this.AccountStatus = problem ?? "Signed in successfully. The service will run as this account.";
        }

        // ----- engine ---------------------------------------------------------------

        private void StartPlan(LaunchAction action)
        {
            this.plannedAction = action;
            this.State = SetupState.Planning;
            this.Validation = null;

            var e = this.Engine;
            if (action == LaunchAction.Install && this.olderVersion == null)
            {
                e.SetVariableString("EIDOS_INSTALLDIR", this.installDir, false);
                e.SetVariableString("EIDOS_DATADIR", this.dataDir, false);
                e.SetVariableString("EIDOS_BIND", this.bind, false);
                e.SetVariableString("EIDOS_PORT", this.port, false);
                if (this.PerMachine)
                {
                    this.SetServiceAccountVariables();
                }
            }
            else if (action == LaunchAction.Install && this.PerMachine && this.account == AccountKind.User)
            {
                // A major upgrade recreates the service. The account name is
                // remembered, but Windows cannot return its password.
                this.SetServiceAccountVariables();
            }
            if (action == LaunchAction.Install || action == LaunchAction.Modify || action == LaunchAction.Repair)
            {
                // Unlike the paths and service account, these Boolean choices
                // are represented by conditioned components rather than MSI
                // registry searches. Pass the detected value on maintenance
                // and upgrade so an absent component remains absent.
                e.SetVariableString("EIDOS_START_MENU", this.startMenu ? "1" : "0", false);
                e.SetVariableString("EIDOS_START_SERVICE", this.startService ? "1" : "0", false);
            }
            if (action == LaunchAction.Uninstall)
            {
                e.SetVariableString("EIDOS_REMOVE_DATA", this.removeData ? "1" : "0", false);
                e.SetVariableString("EIDOS_REMOVE_COLLECTOR", this.removeCollector ? "1" : "0", false);
                e.SetVariableString("EIDOS_COLLECTOR_REMOVE_DATA", this.removeCollectorData ? "1" : "0", false);
            }
            e.SetVariableString("EIDOS_INSTALL_COLLECTOR", this.installCollector ? "1" : "0", false);
            if (!this.PerMachine && (action == LaunchAction.Uninstall || action == LaunchAction.Repair || this.olderVersion != null))
            {
                // The MSI also closes eidos.exe, but stopping it here first
                // keeps the executable and catalog unlocked for the whole
                // transaction. The catalog is crash-safe.
                this.StopPerUserProcess();
            }
            var scope = this.PerMachine ? BundleScope.PerMachine : BundleScope.PerUser;
            e.Plan(action, scope);
        }

        private void SetServiceAccountVariables()
        {
            var kind = this.account == AccountKind.LocalSystem ? "local-system"
                : this.account == AccountKind.LocalService ? "local-service"
                : this.account == AccountKind.NetworkService ? "network-service" : "user";
            this.Engine.SetVariableString("EIDOS_SERVICE_ACCOUNT_KIND", kind, false);
            if (this.account == AccountKind.User)
            {
                var name = this.accountUser;
                var slash = name.IndexOf('\\');
                var domain = slash > 0 ? name.Substring(0, slash) : ".";
                var user = slash > 0 ? name.Substring(slash + 1) : name;
                this.Engine.SetVariableString("EIDOS_SERVICE_DOMAIN", domain, false);
                this.Engine.SetVariableString("EIDOS_SERVICE_USER", user, false);
                this.Engine.SetVariableString("EIDOS_SERVICE_PASSWORD", this.password ?? new SecureString(), false);
            }
        }

        private void OnDetectBegin(object sender, DetectBeginEventArgs e)
        {
            this.Installed = e.RegistrationType == RegistrationType.Full;
        }

        private void OnDetectRelatedBundle(object sender, DetectRelatedBundleEventArgs e)
        {
            if (e.RelationType != RelationType.Upgrade)
            {
                return;
            }
            if (this.IsCollectorBundle(e.ProductCode, e.PerMachine))
            {
                this.collectorRelatedBundles.Add(e.ProductCode);
                this.CollectorInstalled = true;
                this.InstallCollector = true;
                return;
            }
            if (!this.rememberedCore)
            {
                return;
            }
            if (!string.IsNullOrEmpty(this.rememberedCoreVersion))
            {
                // The core MSI's owned registry value distinguishes it from
                // the additional collector-only RelatedBundle relationship.
                return;
            }
            // RelatedBundle also adopts the legacy collector-only bundle.
            // Only a remembered core installation makes this a core upgrade;
            // a collector by itself must not block or choose the core's scope.
            if (this.Engine.CompareVersions(this.Version, e.Version) >= 0
                && (this.olderVersion == null || this.Engine.CompareVersions(e.Version, this.olderVersion) > 0))
            {
                this.olderVersion = e.Version;
            }
        }

        private bool IsCollectorBundle(string bundleCode, bool perMachine)
        {
            try
            {
                var root = perMachine ? Registry.LocalMachine : Registry.CurrentUser;
                using (var key = root.OpenSubKey(UninstallRegistryKey + bundleCode))
                {
                    var upgradeCodes = key?.GetValue("BundleUpgradeCode") as string[];
                    if (upgradeCodes != null)
                    {
                        foreach (var code in upgradeCodes)
                        {
                            if (string.Equals(code?.Trim('{', '}'), CollectorBundleUpgradeCode, StringComparison.OrdinalIgnoreCase))
                            {
                                return true;
                            }
                        }
                    }
                    return string.Equals(key?.GetValue("DisplayName") as string, "eidos observatory collector", StringComparison.OrdinalIgnoreCase);
                }
            }
            catch
            {
                return false;
            }
        }

        private void OnDetectCompatibleMsiPackage(object sender, DetectCompatibleMsiPackageEventArgs e)
        {
            if (e.PackageId == "EidosMsi")
            {
                this.newerInstalled = true;
            }
            else if (e.PackageId == "EidosCollectorMsi")
            {
                this.CollectorInstalled = true;
                this.InstallCollector = true;
            }
        }

        private void OnDetectComplete(object sender, DetectCompleteEventArgs e)
        {
            var cmd = this.ba.Command;
            if (e.Status < 0)
            {
                this.Fail(e.Status, null);
                if (cmd.Display != Display.Full)
                {
                    this.EndNonInteractive();
                }
                this.Requery();
                return;
            }
            this.State = SetupState.Ready;

            if (this.newerInstalled && !this.Installed)
            {
                // MSI error 1638. Set it before showing the blocking page so
                // closing the full UI and quiet/passive runs all fail rather
                // than reporting a successful no-op.
                this.ExitCode = ErrorProductVersion;
                this.State = SetupState.Failed;
                this.Page = Page.Blocked;
                if (cmd.Display != Display.Full)
                {
                    this.EndNonInteractive();
                }
                this.Requery();
                return;
            }

            if (cmd.Display != Display.Full)
            {
                // Silent/passive: EIDOS_SCOPE=perMachine|perUser selects the scope.
                var scopeVar = this.Variable("EIDOS_SCOPE");
                var wantCollector = this.Variable("EIDOS_INSTALL_COLLECTOR");
                var accountKindVar = this.Variable("EIDOS_SERVICE_ACCOUNT_KIND");
                var startServiceVar = this.Variable("EIDOS_START_SERVICE");
                var startMenuVar = this.Variable("EIDOS_START_MENU");
                var removeDataVar = this.Variable("EIDOS_REMOVE_DATA");
                var collectorStartVar = this.Variable("EIDOS_COLLECTOR_START");
                var removeCollectorVar = this.Variable("EIDOS_REMOVE_COLLECTOR");
                var removeCollectorDataVar = this.Variable("EIDOS_COLLECTOR_REMOVE_DATA");
                if (!this.ValidateNonInteractiveChoice("EIDOS_SCOPE", scopeVar, "perUser", "perMachine")
                    || !this.ValidateNonInteractiveChoice("EIDOS_INSTALL_COLLECTOR", wantCollector, "0", "1")
                    || !this.ValidateNonInteractiveChoice("EIDOS_SERVICE_ACCOUNT_KIND", accountKindVar, "local-system", "local-service", "network-service", "user")
                    || !this.ValidateNonInteractiveChoice("EIDOS_START_SERVICE", startServiceVar, "0", "1")
                    || !this.ValidateNonInteractiveChoice("EIDOS_START_MENU", startMenuVar, "0", "1")
                    || !this.ValidateNonInteractiveChoice("EIDOS_REMOVE_DATA", removeDataVar, "0", "1")
                    || !this.ValidateNonInteractiveChoice("EIDOS_COLLECTOR_START", collectorStartVar, "0", "1")
                    || !this.ValidateNonInteractiveChoice("EIDOS_REMOVE_COLLECTOR", removeCollectorVar, "0", "1")
                    || !this.ValidateNonInteractiveChoice("EIDOS_COLLECTOR_REMOVE_DATA", removeCollectorDataVar, "0", "1"))
                {
                    return;
                }
                // Maintenance cannot change the installed core's scope. For a
                // fresh install, scope controls only the dual-scope core; the
                // optional collector remains a per-machine package and can
                // elevate alongside a per-user core.
                this.PerMachine = this.Installed || this.rememberedCore
                    ? this.detectedPerMachine
                    : cmd.Scope == BundleScope.PerMachine
                        || string.Equals(scopeVar, "perMachine", StringComparison.OrdinalIgnoreCase);
                if (!string.IsNullOrEmpty(accountKindVar))
                {
                    // Command-line choices are already stored in Burn for MSI
                    // forwarding, but the BA must use the same effective value
                    // for its pre-plan named-account password check.
                    this.account = string.Equals(accountKindVar, "local-system", StringComparison.OrdinalIgnoreCase) ? AccountKind.LocalSystem
                        : string.Equals(accountKindVar, "local-service", StringComparison.OrdinalIgnoreCase) ? AccountKind.LocalService
                        : string.Equals(accountKindVar, "network-service", StringComparison.OrdinalIgnoreCase) ? AccountKind.NetworkService
                        : AccountKind.User;
                }
                // The collector: 1 installs or keeps it, 0 leaves it out or
                // removes it during install/modify, and empty keeps whatever
                // is detected. Removal keeps the service only when asked
                // (EIDOS_REMOVE_COLLECTOR=0) and its data unless
                // EIDOS_COLLECTOR_REMOVE_DATA=1.
                this.installCollector = wantCollector == "1" || (this.collectorInstalled && wantCollector != "0");
                if (!string.IsNullOrEmpty(startServiceVar))
                {
                    this.startService = startServiceVar == "1";
                }
                else if (this.rememberedCore)
                {
                    this.Engine.SetVariableString("EIDOS_START_SERVICE", this.startService ? "1" : "0", false);
                }
                if (!string.IsNullOrEmpty(startMenuVar))
                {
                    this.startMenu = startMenuVar == "1";
                }
                else if (this.rememberedCore)
                {
                    this.Engine.SetVariableString("EIDOS_START_MENU", this.startMenu ? "1" : "0", false);
                }
                if (cmd.Action == LaunchAction.Install && !this.Installed && this.olderVersion != null
                    && this.PerMachine && this.account == AccountKind.User
                    && string.IsNullOrEmpty(this.Variable("EIDOS_SERVICE_PASSWORD")))
                {
                    this.Fail(ErrorLogonFailure, "This upgrade must recreate a service that runs as a Windows user. Run setup again with EIDOS_SERVICE_PASSWORD set for that account.");
                    this.EndNonInteractive();
                    this.Requery();
                    return;
                }
                this.removeCollector = string.IsNullOrEmpty(removeCollectorVar) || removeCollectorVar == "1";
                this.removeCollectorData = removeCollectorDataVar == "1";
                this.installDirEdited = this.dataDirEdited = true; // overridable variables win
                this.plannedAction = cmd.Action;
                this.State = SetupState.Planning;
                this.Engine.Plan(cmd.Action, this.PerMachine ? BundleScope.PerMachine : BundleScope.PerUser);
                return;
            }

            if (cmd.Action == LaunchAction.Uninstall)
            {
                this.PerMachine = this.detectedPerMachine;
                this.ComputeDataSize();
                this.Page = Page.Remove;
            }
            else if (this.Installed)
            {
                this.PerMachine = this.detectedPerMachine;
                this.ComputeDataSize();
                this.Page = Page.Maintenance;
            }
            else
            {
                if (this.olderVersion != null)
                {
                    // Show the correct elevation note and plan the fixed scope
                    // before the operator confirms the upgrade.
                    this.PerMachine = this.detectedPerMachine;
                }
                this.Page = Page.Welcome;
            }
            this.Requery();
        }

        private string Variable(string name)
        {
            return this.Engine.ContainsVariable(name) ? (this.Engine.GetVariableString(name) ?? "") : "";
        }

        private bool ValidateNonInteractiveChoice(string name, string value, params string[] allowed)
        {
            if (string.IsNullOrEmpty(value))
            {
                return true;
            }
            foreach (var choice in allowed)
            {
                if (string.Equals(value, choice, StringComparison.OrdinalIgnoreCase))
                {
                    return true;
                }
            }
            this.Fail(ErrorInvalidArgument, $"{name} must be empty or one of: {string.Join(", ", allowed)}.");
            this.EndNonInteractive();
            this.Requery();
            return false;
        }

        private void OnDetectPackageComplete(object sender, DetectPackageCompleteEventArgs e)
        {
            if (e.PackageId == "EidosMsi" && e.State == PackageState.Obsolete)
            {
                this.newerInstalled = true;
            }
            else if (e.PackageId == "EidosCollectorMsi"
                && (e.State == PackageState.Present || e.State == PackageState.Obsolete || e.State == PackageState.Superseded))
            {
                this.CollectorInstalled = true;
                // Maintenance and upgrades start from the installed state.
                this.InstallCollector = true;
            }
        }

        private void OnPlanPackageBegin(object sender, PlanPackageBeginEventArgs e)
        {
            if (e.PackageId == "EidosCollectorMsi")
            {
                switch (this.plannedAction)
                {
                    case LaunchAction.Uninstall:
                    case LaunchAction.UnsafeUninstall:
                        e.State = this.collectorInstalled && this.removeCollector ? RequestState.Absent : RequestState.None;
                        break;
                    case LaunchAction.Repair:
                        e.State = this.installCollector
                            ? e.CurrentState == PackageState.Present
                                ? RequestState.Repair
                                : e.CurrentState == PackageState.Obsolete || e.CurrentState == PackageState.Superseded
                                    ? RequestState.None
                                    : RequestState.Present
                            : this.collectorInstalled ? RequestState.Absent : RequestState.None;
                        break;
                    case LaunchAction.Install:
                    case LaunchAction.Modify:
                        // Install or upgrade: present when chosen (or already
                        // there and not unticked); otherwise untouched, never
                        // removed as a side effect of an empty choice.
                        e.State = this.installCollector
                            ? e.CurrentState == PackageState.Obsolete || e.CurrentState == PackageState.Superseded
                                ? RequestState.None
                                : RequestState.Present
                            : this.collectorInstalled ? RequestState.Absent : RequestState.None;
                        break;
                    default:
                        // Layout, cache and update actions keep Burn's
                        // action-specific recommendation.
                        break;
                }
                return;
            }
            // The .NET Framework prerequisite exists for the fallback BA that
            // runs when this UI cannot start. If we are running, it is
            // satisfied: leave it out of the plan so a per-user install never
            // touches the per-machine package cache (which needs elevation).
            if (e.PackageId.StartsWith("NetFx", StringComparison.OrdinalIgnoreCase)
                && (this.plannedAction == LaunchAction.Install || this.plannedAction == LaunchAction.Modify
                    || this.plannedAction == LaunchAction.Repair || this.plannedAction == LaunchAction.Uninstall
                    || this.plannedAction == LaunchAction.UnsafeUninstall))
            {
                e.State = RequestState.None;
            }
        }

        private void OnPlanCompatibleMsiPackageBegin(object sender, PlanCompatibleMsiPackageBeginEventArgs e)
        {
            if (e.PackageId == "EidosCollectorMsi")
            {
                // Burn recommends removing a newer compatible MSI on every
                // bundle uninstall. The collector is independently optional,
                // so honor the operator's keep/remove choice here too.
                e.RequestRemove = ((this.plannedAction == LaunchAction.Uninstall || this.plannedAction == LaunchAction.UnsafeUninstall)
                        && this.removeCollector)
                    || ((this.plannedAction == LaunchAction.Install || this.plannedAction == LaunchAction.Modify || this.plannedAction == LaunchAction.Repair)
                        && !this.installCollector);
            }
        }

        private void OnPlanRelatedBundleType(object sender, PlanRelatedBundleTypeEventArgs e)
        {
            if (e.RecommendedType == RelatedBundlePlanType.Downgrade
                && this.collectorRelatedBundles.Contains(e.BundleCode))
            {
                var removing = (this.plannedAction == LaunchAction.Uninstall || this.plannedAction == LaunchAction.UnsafeUninstall)
                    ? this.removeCollector
                    : !this.installCollector;
                // A newer independently packaged collector must not turn a
                // core install into Burn's bundle-wide downgrade no-op. Keep
                // it unrelated, unless the operator explicitly removes it.
                e.Type = removing ? RelatedBundlePlanType.Upgrade : RelatedBundlePlanType.None;
            }
        }

        private void OnPlanComplete(object sender, PlanCompleteEventArgs e)
        {
            if (e.Status >= 0)
            {
                this.Progress = 0;
                this.ProgressMessage = "Preparing…";
                this.State = SetupState.Applying;
                this.Page = Page.Progress;
                this.Engine.Apply(this.WindowHandle);
            }
            else
            {
                this.Fail(e.Status, null);
            }
        }

        private void OnApplyBegin(object sender, ApplyBeginEventArgs e)
        {
            this.Canceled = false;
            this.collectorError = null;
        }

        private void OnProgress(object sender, ProgressEventArgs e)
        {
            e.Cancel = this.Canceled;
        }

        private void OnCacheProgress(object sender, CacheAcquireProgressEventArgs e)
        {
            e.Cancel = this.Canceled;
        }

        private void OnExecuteProgress(object sender, ExecuteProgressEventArgs e)
        {
            this.Progress = e.OverallPercentage;
            e.Cancel = this.Canceled;
        }

        private void OnExecutePackageBegin(object sender, ExecutePackageBeginEventArgs e)
        {
            this.ProgressMessage = this.plannedAction == LaunchAction.Uninstall ? "Removing eidos…" : "Installing eidos…";
            e.Cancel = this.Canceled;
        }

        private void OnExecutePackageComplete(object sender, ExecutePackageCompleteEventArgs e)
        {
            if (e.PackageId == "EidosCollectorMsi" && e.Status < 0)
            {
                // The collector is non-vital so its MSI failure does not roll
                // back a healthy core. Remember it so the BA still reports the
                // requested optional-package operation as a failure.
                this.collectorError = e.Status;
            }
        }

        private void OnExecuteMsiMessage(object sender, ExecuteMsiMessageEventArgs e)
        {
            // ActionStart messages carry a readable phase description.
            if (e.MessageType == InstallMessage.ActionStart && !string.IsNullOrWhiteSpace(e.Message))
            {
                var text = e.Message;
                var colon = text.IndexOf(": ");
                if (colon > 0 && colon < 30)
                {
                    text = text.Substring(colon + 2);
                }
                this.ProgressMessage = text;
            }
        }

        private void OnError(object sender, WixToolset.BootstrapperApplicationApi.ErrorEventArgs e)
        {
            if (this.Canceled)
            {
                e.Result = Result.Cancel;
                return;
            }
            if (!string.IsNullOrWhiteSpace(e.ErrorMessage))
            {
                this.ErrorMessage = e.ErrorMessage.Trim();
            }
            // Let the engine's recommendation stand; the message is shown on the failure page.
        }

        private void OnApplyComplete(object sender, ApplyCompleteEventArgs e)
        {
            this.ExitCode = e.Status;
            this.RestartRequired = e.Restart != ApplyRestart.None;
            if (e.Status >= 0 && !this.collectorError.HasValue)
            {
                if ((this.plannedAction == LaunchAction.Install || this.plannedAction == LaunchAction.Modify || this.plannedAction == LaunchAction.Repair)
                    && !this.PerMachine && this.startService)
                {
                    // Per-user has no service: start the background process
                    // now (the Run entry does it at the next sign-in).
                    if (!this.StartPerUserProcess())
                    {
                        this.Fail(ErrorServiceNotActive, this.ErrorMessage);
                        if (this.ba.Command.Display != Display.Full)
                        {
                            this.EndNonInteractive();
                            return;
                        }
                        this.Requery();
                        return;
                    }
                }
                this.State = SetupState.Applied;
                this.Page = Page.Success;
                if (this.ba.Command.Display != Display.Full)
                {
                    this.EndNonInteractive();
                    return;
                }
                this.Raise(nameof(this.SuccessText), nameof(this.PrimaryLabel));
            }
            else
            {
                var status = e.Status < 0 ? e.Status : this.collectorError.Value;
                var message = e.Status < 0
                    ? this.ErrorMessage
                    : this.plannedAction == LaunchAction.Uninstall
                        ? "eidos was removed, but the profiling collector could not be removed. The setup log has the details."
                        : "The eidos core operation completed, but the profiling collector operation failed. The setup log has the details.";
                this.Fail(status, message);
                if (this.ba.Command.Display != Display.Full)
                {
                    this.EndNonInteractive();
                    return;
                }
            }
            this.Requery();
        }

        /// <summary>End every eidos.exe running from the install folder.</summary>
        private void StopPerUserProcess()
        {
            var dir = this.installDir.TrimEnd('\\');
            foreach (var p in System.Diagnostics.Process.GetProcessesByName("eidos"))
            {
                try
                {
                    var path = p.MainModule?.FileName ?? "";
                    if (path.StartsWith(dir, StringComparison.OrdinalIgnoreCase))
                    {
                        this.Engine.Log(LogLevel.Standard, $"stopping eidos.exe (pid {p.Id}) before {this.plannedAction}");
                        p.Kill();
                        p.WaitForExit(15_000);
                    }
                }
                catch (Exception ex)
                {
                    this.Engine.Log(LogLevel.Error, $"stopping eidos.exe (pid {p.Id}): {ex.Message}");
                }
                finally
                {
                    p.Dispose();
                }
            }
        }

        /// <summary>
        /// `eidos serve --detach` launches the background process and returns
        /// once /api/health answers (or if something already answers there).
        /// </summary>
        private bool StartPerUserProcess()
        {
            try
            {
                // The MSI wrote what it actually used (quiet installs take
                // their values from bundle variables, not from this model).
                this.ReadRememberedSettings();
                this.Raise(nameof(this.InstallDir), nameof(this.DataDir), nameof(this.Port), nameof(this.Bind), nameof(this.Url));
                var exe = Path.Combine(this.installDir, "eidos.exe");
                var data = this.dataDir.TrimEnd('\\');
                var args = $"serve --detach --data-dir \"{data}\" --log-dir \"{Path.Combine(data, "logs")}\" --bind {this.bind}:{this.port}";
                this.ProgressMessage = "Starting eidos…";
                // No output redirection: the background eidos process the
                // launcher spawns would inherit the pipe and keep it open,
                // and a read-to-end here would never return.
                using (var p = new System.Diagnostics.Process())
                {
                    p.StartInfo.FileName = exe;
                    p.StartInfo.Arguments = args;
                    p.StartInfo.UseShellExecute = false;
                    p.StartInfo.CreateNoWindow = true;
                    p.Start();
                    if (!p.WaitForExit(90_000))
                    {
                        p.Kill();
                        this.Engine.Log(LogLevel.Error, "eidos serve --detach did not return within 90 s");
                        this.ErrorMessage = "eidos was installed but did not start within 90 seconds. The setup log has the details.";
                        return false;
                    }
                    this.Engine.Log(LogLevel.Standard, $"eidos serve --detach exited with {p.ExitCode}");
                    if (p.ExitCode != 0)
                    {
                        this.ErrorMessage = $"eidos could not be started (exit code {p.ExitCode}); see the log in {Path.Combine(data, "logs")}.";
                        return false;
                    }
                }
                return true;
            }
            catch (Exception ex)
            {
                this.Engine.Log(LogLevel.Error, "starting eidos: " + ex.Message);
                this.ErrorMessage = "eidos was installed but could not be started. The setup log has the details.";
                return false;
            }
        }

        /// <summary>
        /// Passive mode has a visible window to close; quiet/embedded mode
        /// never showed one, and closing an unshown window does not end the
        /// dispatcher loop, so shut it down directly (as WixBA does).
        /// </summary>
        private void EndNonInteractive()
        {
            if (this.Dispatcher == null)
            {
                return;
            }
            if (this.ba.Command.Display == Display.Passive)
            {
                this.Dispatcher.BeginInvoke(new Action(() => EidosBootstrapper.View?.Close()));
            }
            else
            {
                this.Dispatcher.BeginInvoke(new Action(() => this.Dispatcher.InvokeShutdown()));
            }
        }

        private void Fail(int status, string message)
        {
            this.ExitCode = status;
            this.State = SetupState.Failed;
            if (this.Canceled)
            {
                this.ErrorMessage = "Nothing was changed.";
            }
            else if (string.IsNullOrEmpty(message))
            {
                this.ErrorMessage = $"Error 0x{status:X8}. The log has the details.";
            }
            else
            {
                this.ErrorMessage = message;
            }
            this.Page = Page.Failure;
            this.Raise(nameof(this.PageTitle));
        }

        private void ComputeDataSize()
        {
            var dir = this.dataDir;
            if (string.IsNullOrEmpty(dir) || !Directory.Exists(dir))
            {
                return;
            }
            Task.Run(() =>
            {
                long total = 0;
                try
                {
                    foreach (var f in Directory.EnumerateFiles(dir, "*", SearchOption.AllDirectories))
                    {
                        try { total += new FileInfo(f).Length; } catch { }
                    }
                }
                catch { }
                this.dataSize = total >= 1L << 30 ? $"{total / (double)(1L << 30):0.0} GB" : total >= 1L << 20 ? $"{total / (double)(1L << 20):0} MB" : $"{total / 1024.0:0} KB";
                this.Raise(nameof(this.RemoveDataLabel));
            });
        }

        private void Requery()
        {
            this.Dispatcher?.BeginInvoke(new Action(CommandManager.InvalidateRequerySuggested));
        }

        private bool Set<T>(ref T field, T value, [CallerMemberName] string name = null)
        {
            if (Equals(field, value))
            {
                return false;
            }
            field = value;
            this.PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
            return true;
        }

        private void Raise(params string[] names)
        {
            foreach (var n in names)
            {
                this.PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(n));
            }
        }
    }
}
