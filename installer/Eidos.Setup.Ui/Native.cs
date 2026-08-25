using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Security;
using System.Security.Principal;

namespace Eidos.Setup
{
    /// <summary>Small Win32 helpers used by the setup pages.</summary>
    internal static class Native
    {
        private const int LOGON32_LOGON_NETWORK = 3;
        private const int LOGON32_PROVIDER_DEFAULT = 0;

        [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        private static extern bool LogonUserW(string user, string domain, IntPtr password, int logonType, int logonProvider, out IntPtr token);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool CloseHandle(IntPtr handle);

        /// <summary>
        /// Check a user name and password without needing any logon right on
        /// this machine (a network logon is enough to prove the credentials).
        /// Returns null on success, otherwise a message for the user.
        /// </summary>
        public static string ValidateCredentials(string domain, string user, SecureString password)
        {
            if (string.IsNullOrWhiteSpace(user))
            {
                return "Enter the account name.";
            }
            if (password == null || password.Length == 0)
            {
                return "Enter the account's password.";
            }
            var passwordPtr = IntPtr.Zero;
            try
            {
                passwordPtr = Marshal.SecureStringToGlobalAllocUnicode(password);
                if (LogonUserW(user, string.IsNullOrEmpty(domain) ? "." : domain, passwordPtr, LOGON32_LOGON_NETWORK, LOGON32_PROVIDER_DEFAULT, out var token))
                {
                    CloseHandle(token);
                    return null;
                }
                var error = Marshal.GetLastWin32Error();
                var text = new Win32Exception(error).Message;
                return error == 1326 ? "The user name or password is incorrect." : text;
            }
            finally
            {
                if (passwordPtr != IntPtr.Zero)
                {
                    Marshal.ZeroFreeGlobalAllocUnicode(passwordPtr);
                }
            }
        }

        /// <summary>DOMAIN\user of the interactive user.</summary>
        public static string CurrentUser()
        {
            try
            {
                return WindowsIdentity.GetCurrent().Name;
            }
            catch
            {
                return Environment.UserDomainName + "\\" + Environment.UserName;
            }
        }

        public static bool IsAdministrator()
        {
            try
            {
                return new WindowsPrincipal(WindowsIdentity.GetCurrent()).IsInRole(WindowsBuiltInRole.Administrator);
            }
            catch
            {
                return false;
            }
        }

        /// <summary>
        /// True when nothing is listening on the address. A false result is
        /// definitive; a true result only means "free right now".
        /// </summary>
        public static bool IsPortFree(string bind, int port)
        {
            if (!IPAddress.TryParse(bind, out var address))
            {
                address = IPAddress.Loopback;
            }
            try
            {
                var listener = new TcpListener(address, port);
                listener.Start();
                listener.Stop();
                return true;
            }
            catch (SocketException)
            {
                return false;
            }
        }

        public static void ShellOpen(string target)
        {
            try
            {
                using (var p = new Process())
                {
                    p.StartInfo.FileName = target;
                    p.StartInfo.UseShellExecute = true;
                    p.Start();
                }
            }
            catch
            {
                // Best effort: a missing browser association is not a setup failure.
            }
        }
    }
}
