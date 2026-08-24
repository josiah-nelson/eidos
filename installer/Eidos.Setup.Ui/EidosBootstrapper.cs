using System.Windows.Threading;
using WixToolset.BootstrapperApplicationApi;

namespace Eidos.Setup
{
    /// <summary>
    /// The bootstrapper application Burn launches for eidos-setup.exe.
    /// </summary>
    public sealed class EidosBootstrapper : BootstrapperApplication
    {
        public static SetupModel Model { get; private set; }
        public static MainWindow View { get; private set; }
        public static Dispatcher Dispatcher { get; private set; }

        public IBootstrapperCommand Command { get; private set; }
        public IEngine Engine => this.engine;

        protected override void OnCreate(CreateEventArgs args)
        {
            base.OnCreate(args);
            this.Command = args.Command;
        }

        protected override void Run()
        {
            this.Engine.Log(LogLevel.Standard, "eidos setup UI starting");
            Model = new SetupModel(this);
            Dispatcher = Dispatcher.CurrentDispatcher;
            Model.Dispatcher = Dispatcher;
            View = new MainWindow(Model);

            if (this.Command.Display == Display.Full || this.Command.Display == Display.Passive)
            {
                View.Show();
            }

            this.Engine.Detect();
            Dispatcher.Run();

            var exit = Model.ExitCode;
            if ((exit & 0xFFFF0000) == unchecked((int)0x80070000))
            {
                exit &= 0xFFFF; // plain Win32 code, not an HRESULT
            }
            this.Engine.Quit(exit);
        }
    }
}
