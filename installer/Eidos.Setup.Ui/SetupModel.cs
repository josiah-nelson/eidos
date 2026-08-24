using System;
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
        private const string RegistryKey = @"Software\eidos";

        private readonly EidosBootstrapper ba;
        private Page page = Page.Loading;
        private SetupState state = SetupState.Detecting;
        private bool installed;
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
        private bool repair = true;
        private string validation;
        private int progress;
        private string progressMessage = "";
        private string errorMessage;
        private bool canceled;
        private bool restartRequired;
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

            ba.DetectBegin += this.OnDetectBegin;
            ba.DetectRelatedBundle += this.OnDetectRelatedBundle;
            ba.DetectComplete += this.OnDetectComplete;
            ba.PlanComplete += this.OnPlanComplete;
            ba.ApplyBegin += this.OnApplyBegin;
            ba.Progress += this.OnProgress;
            ba.CacheAcquireProgress += this.OnCacheProgress;
            ba.ExecuteProgress += this.OnExecuteProgress;
            ba.ExecutePackageBegin += this.OnExecutePackageBegin;
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
            this.OpenEidosCommand = new RelayCommand(_ => Native.ShellOpen(this.Url));
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
                    case Page.Welcome: return this.olderVersion != null ? "Upgrade" : "Next";
                    case Page.Options: return this.PerMachine && !Native.IsAdministrator() ? "Install (administrator approval)" : "Install";
                    case Page.Maintenance: return "Next";
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

        // ----- choices ----------------------------------------------------------

        public bool PerMachine
        {
            get => this.perMachine;
            set
            {
                if (this.Set(ref this.perMachine, value))
                {
                    this.Raise(nameof(this.PerUser), nameof(this.PrimaryLabel), nameof(this.ElevationNote), nameof(this.SummaryText));
                    this.ApplyScopeDefaults();
                }
            }
        }

        public bool PerUser
        {
            get => !this.perMachine;
            set => this.PerMachine = !value;
        }

        public string ElevationNote => this.PerMachine && !Native.IsAdministrator()
            ? "Windows will ask for administrator approval when the installation starts."
            : "";

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
                    this.Raise(nameof(this.BindWarning), nameof(this.Url), nameof(this.SummaryText));
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
                    this.Raise(nameof(this.Url), nameof(this.SummaryText));
                }
            }
        }

        public string Url
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
                    case AccountKind.LocalService:
                        return "Least privilege. Sees local files that Everyone can read; the data folder is granted explicitly. Anonymous on the network.";
                    case AccountKind.NetworkService:
                        return "Least privilege locally; presents the computer's identity on the network, which works for shares that trust this computer's account.";
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
        public bool StartMenu { get => this.startMenu; set { if (this.Set(ref this.startMenu, value)) this.Raise(nameof(this.SummaryText)); } }
        public bool LaunchAfter { get => this.launchAfter; set { if (this.Set(ref this.launchAfter, value)) this.Raise(nameof(this.PrimaryLabel)); } }
        public bool RemoveData { get => this.removeData; set => this.Set(ref this.removeData, value); }
        public bool Repair { get => this.repair; set { if (this.Set(ref this.repair, value)) this.Raise(nameof(this.Uninstall)); } }
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
                lines += $"\nStart menu:\t{(this.startMenu ? "eidos shortcuts" : "none")}";
                return lines;
            }
        }

        // ----- progress / result -----------------------------------------------

        public int Progress { get => this.progress; private set => this.Set(ref this.progress, value); }
        public string ProgressMessage { get => this.progressMessage; private set => this.Set(ref this.progressMessage, value); }
        public string ErrorMessage { get => this.errorMessage; private set => this.Set(ref this.errorMessage, value); }
        public bool RestartRequired { get => this.restartRequired; private set => this.Set(ref this.restartRequired, value); }
        public string SuccessText => this.plannedAction == LaunchAction.Uninstall
            ? (this.removeData ? "The program and its data were removed." : $"The program was removed. Your indexed data is still in {this.dataDir}; delete that folder if you no longer want it.")
            : $"eidos is running at {this.Url}.\n{(this.PerMachine ? "The service starts with Windows." : "Use the Start menu entry \"Start eidos\" to run it; it stops when you sign out.")}";

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
                        // Upgrade: the MSI reads the remembered settings; keep the scope.
                        this.PerMachine = this.detectedPerMachine;
                        this.StartPlan(LaunchAction.Install);
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
                        this.Page = Page.Options;
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
                        Native.ShellOpen(this.Url);
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
                case Page.Account: this.Page = Page.Location; break;
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
                        this.detectedPerMachine = root == Registry.LocalMachine;
                        return;
                    }
                }
                catch
                {
                    // unreadable hive: fall back to defaults
                }
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
                e.SetVariableString("EIDOS_START_MENU", this.startMenu ? "1" : "0", false);
                if (this.PerMachine)
                {
                    var kind = this.account == AccountKind.LocalSystem ? "local-system"
                        : this.account == AccountKind.LocalService ? "local-service"
                        : this.account == AccountKind.NetworkService ? "network-service" : "user";
                    e.SetVariableString("EIDOS_SERVICE_ACCOUNT_KIND", kind, false);
                    e.SetVariableString("EIDOS_START_SERVICE", this.startService ? "1" : "0", false);
                    if (this.account == AccountKind.User)
                    {
                        var name = this.accountUser;
                        var slash = name.IndexOf('\\');
                        var domain = slash > 0 ? name.Substring(0, slash) : ".";
                        var user = slash > 0 ? name.Substring(slash + 1) : name;
                        e.SetVariableString("EIDOS_SERVICE_DOMAIN", domain, false);
                        e.SetVariableString("EIDOS_SERVICE_USER", user, false);
                        e.SetVariableString("EIDOS_SERVICE_PASSWORD", this.password ?? new SecureString(), false);
                    }
                }
            }
            if (action == LaunchAction.Uninstall)
            {
                e.SetVariableString("EIDOS_REMOVE_DATA", this.removeData ? "1" : "0", false);
            }
            var scope = this.PerMachine ? BundleScope.PerMachine : BundleScope.PerUser;
            e.Plan(action, scope);
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
            if (this.Engine.CompareVersions(this.Version, e.Version) > 0)
            {
                this.olderVersion = e.Version;
                this.detectedPerMachine = e.PerMachine;
            }
            else
            {
                this.newerInstalled = true;
            }
        }

        private void OnDetectComplete(object sender, DetectCompleteEventArgs e)
        {
            var cmd = this.ba.Command;
            this.State = SetupState.Ready;

            if (cmd.Display != Display.Full)
            {
                // Silent/passive: EIDOS_SCOPE=perMachine|perUser selects the scope.
                var scopeVar = this.Engine.ContainsVariable("EIDOS_SCOPE") ? this.Engine.GetVariableString("EIDOS_SCOPE") : "";
                this.PerMachine = cmd.Scope == BundleScope.PerMachine
                    || string.Equals(scopeVar, "perMachine", StringComparison.OrdinalIgnoreCase)
                    || (this.Installed && this.detectedPerMachine);
                this.installDirEdited = this.dataDirEdited = true; // overridable variables win
                this.plannedAction = cmd.Action;
                this.State = SetupState.Planning;
                this.Engine.Plan(cmd.Action, this.PerMachine ? BundleScope.PerMachine : BundleScope.PerUser);
                return;
            }

            if (this.newerInstalled && !this.Installed)
            {
                this.Page = Page.Blocked;
            }
            else if (cmd.Action == LaunchAction.Uninstall)
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
                this.Page = Page.Welcome;
            }
            this.Requery();
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
            if (e.Status >= 0)
            {
                this.State = SetupState.Applied;
                this.Page = Page.Success;
                if (this.ba.Command.Display != Display.Full)
                {
                    this.Dispatcher?.BeginInvoke(new Action(() => EidosBootstrapper.View?.Close()));
                    return;
                }
                this.Raise(nameof(this.SuccessText), nameof(this.PrimaryLabel));
            }
            else
            {
                this.Fail(e.Status, this.ErrorMessage);
                if (this.ba.Command.Display != Display.Full)
                {
                    this.Dispatcher?.BeginInvoke(new Action(() => EidosBootstrapper.View?.Close()));
                    return;
                }
            }
            this.Requery();
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
