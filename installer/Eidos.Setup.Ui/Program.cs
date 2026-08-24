using WixToolset.BootstrapperApplicationApi;

namespace Eidos.Setup
{
    internal static class Program
    {
        private static int Main()
        {
            // Burn starts this process and connects to it; everything else
            // happens on the BA thread inside EidosBootstrapper.Run.
            ManagedBootstrapperApplication.Run(new EidosBootstrapper());
            return 0;
        }
    }
}
