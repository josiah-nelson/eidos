using System;
using System.Runtime.InteropServices;

namespace Eidos.Setup
{
    /// <summary>
    /// The modern folder picker (IFileOpenDialog with FOS_PICKFOLDERS); the
    /// WinForms FolderBrowserDialog on .NET Framework is the Windows XP tree.
    /// </summary>
    internal static class FolderPicker
    {
        public static string Pick(IntPtr owner, string title, string initial)
        {
            IFileOpenDialog dialog = null;
            try
            {
                dialog = (IFileOpenDialog)new FileOpenDialogRcw();
                dialog.GetOptions(out var options);
                options |= FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR;
                dialog.SetOptions(options);
                if (!string.IsNullOrEmpty(title))
                {
                    dialog.SetTitle(title);
                }
                if (!string.IsNullOrEmpty(initial))
                {
                    var hr = SHCreateItemFromParsingName(initial, IntPtr.Zero, typeof(IShellItem).GUID, out var item);
                    if (hr == 0 && item != null)
                    {
                        dialog.SetFolder(item);
                        Marshal.ReleaseComObject(item);
                    }
                }
                if (dialog.Show(owner) != 0)
                {
                    return null; // cancelled
                }
                dialog.GetResult(out var result);
                result.GetDisplayName(SIGDN_FILESYSPATH, out var path);
                Marshal.ReleaseComObject(result);
                return path;
            }
            catch (COMException)
            {
                return null;
            }
            finally
            {
                if (dialog != null)
                {
                    Marshal.ReleaseComObject(dialog);
                }
            }
        }

        private const uint FOS_PICKFOLDERS = 0x20;
        private const uint FOS_FORCEFILESYSTEM = 0x40;
        private const uint FOS_NOCHANGEDIR = 0x8;
        private const uint SIGDN_FILESYSPATH = 0x80058000;

        [DllImport("shell32.dll", CharSet = CharSet.Unicode, PreserveSig = true)]
        private static extern int SHCreateItemFromParsingName(string path, IntPtr bindContext, [In] Guid riid, [MarshalAs(UnmanagedType.Interface)] out IShellItem item);

        [ComImport, Guid("DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7")]
        private class FileOpenDialogRcw { }

        [ComImport, Guid("42f85136-db7e-439c-85f1-e4075d135fc8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
        private interface IFileOpenDialog
        {
            [PreserveSig] int Show(IntPtr parent);
            void SetFileTypes(uint count, IntPtr types);
            void SetFileTypeIndex(uint index);
            void GetFileTypeIndex(out uint index);
            void Advise(IntPtr events, out uint cookie);
            void Unadvise(uint cookie);
            void SetOptions(uint options);
            void GetOptions(out uint options);
            void SetDefaultFolder(IShellItem item);
            void SetFolder(IShellItem item);
            void GetFolder(out IShellItem item);
            void GetCurrentSelection(out IShellItem item);
            void SetFileName([MarshalAs(UnmanagedType.LPWStr)] string name);
            void GetFileName([MarshalAs(UnmanagedType.LPWStr)] out string name);
            void SetTitle([MarshalAs(UnmanagedType.LPWStr)] string title);
            void SetOkButtonLabel([MarshalAs(UnmanagedType.LPWStr)] string text);
            void SetFileNameLabel([MarshalAs(UnmanagedType.LPWStr)] string label);
            void GetResult(out IShellItem item);
            void AddPlace(IShellItem item, int where);
            void SetDefaultExtension([MarshalAs(UnmanagedType.LPWStr)] string extension);
            void Close(int hr);
            void SetClientGuid([In] ref Guid guid);
            void ClearClientData();
            void SetFilter(IntPtr filter);
            void GetResults(out IntPtr items);
            void GetSelectedItems(out IntPtr items);
        }

        [ComImport, Guid("43826d1e-e718-42ee-bc55-a1e261c37bfe"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
        private interface IShellItem
        {
            void BindToHandler(IntPtr bindContext, [In] ref Guid handler, [In] ref Guid riid, out IntPtr result);
            void GetParent(out IShellItem parent);
            void GetDisplayName(uint form, [MarshalAs(UnmanagedType.LPWStr)] out string name);
            void GetAttributes(uint mask, out uint attributes);
            void Compare(IShellItem other, uint hint, out int order);
        }
    }
}
