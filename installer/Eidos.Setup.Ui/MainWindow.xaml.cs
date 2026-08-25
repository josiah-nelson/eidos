using System.ComponentModel;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Interop;

namespace Eidos.Setup
{
    public partial class MainWindow : Window
    {
        private readonly SetupModel model;

        public MainWindow(SetupModel model)
        {
            this.model = model;
            this.DataContext = model;
            this.InitializeComponent();
            model.WindowHandle = new WindowInteropHelper(this).EnsureHandle();
            this.Loaded += (s, e) => model.Engine.CloseSplashScreen();
            this.Closed += (s, e) => this.Dispatcher.InvokeShutdown();
        }

        private void OnClosing(object sender, CancelEventArgs e)
        {
            if (this.model.State == SetupState.Applying)
            {
                this.model.Cancel();
                // The engine rolls back and completes; the window closes then.
                e.Cancel = true;
            }
        }

        /// <summary>PasswordBox cannot be bound; copy its SecureString on change.</summary>
        private void OnPasswordChanged(object sender, RoutedEventArgs e)
        {
            if (sender is PasswordBox box)
            {
                this.model.Password = box.SecurePassword;
            }
        }
    }
}
