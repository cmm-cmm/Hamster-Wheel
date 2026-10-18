// Integration shim for the pinned WinSW 2.11.0 NET461 release, not a process manager.
// Compile with Build-LifecycleExtension.ps1; never replace the upstream executable.
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Reflection;
using System.Runtime.ExceptionServices;
using System.Runtime.InteropServices;
using System.ServiceProcess;
using System.Security.Cryptography;
using System.Threading;
using Microsoft.Win32.SafeHandles;
using WinSW.Extensions;

namespace Hamster.WinSW
{
    public sealed class LifecycleExtension : AbstractWinSWExtension
    {
        // A rooted, non-inheritable handle belongs ONLY to this wrapper process.
        // Never dispose after self-assignment: the job also contains the wrapper.
        // OS process teardown, not BeforeWrapperStopped, closes the final handle.
        private static SafeFileHandle lifetimeJob;
        private static IntPtr serviceStatusHandle;
        private static MethodInfo signalStopped;
        private static int reportingStopped;
        public override string DisplayName { get { return "Hamster pinned WinSW lifecycle"; } }

        public override void OnWrapperStarted()
        {
            var wrapperType = typeof(global::WinSW.WrapperService);
            var assembly = wrapperType.Assembly;
            if (assembly.GetName().Name != "WinSW.NET461" ||
                assembly.GetName().Version != new Version(2, 11, 0, 0))
                throw new InvalidOperationException("Lifecycle extension requires WinSW 2.11.0 NET461");
            using (var file = File.OpenRead(assembly.Location))
            using (var hash = SHA256.Create())
                if (BitConverter.ToString(hash.ComputeHash(file)).Replace("-", "") !=
                    "31DCA0D9BD3C5AB7E119172BDB927B90C9DA935D0F4963335EAF60E7D8F7AABF")
                    throw new InvalidOperationException("Lifecycle extension requires the exact pinned WinSW executable");
            if (lifetimeJob != null) throw new InvalidOperationException("Duplicate lifecycle extension");

            // WinSW has no public unsolicited-exit callback. Its own service
            // status handle needs NO SCM manager rights; never OpenSCManager.
            var providerField = wrapperType.GetField("eventLogProvider", BindingFlags.Static | BindingFlags.NonPublic);
            var provider = providerField == null ? null : providerField.GetValue(null);
            var serviceProperty = provider == null ? null : provider.GetType().GetProperty("Service");
            var service = serviceProperty == null ? null : serviceProperty.GetValue(provider, null) as ServiceBase;
            var handleProperty = typeof(ServiceBase).GetProperty("ServiceHandle", BindingFlags.Instance | BindingFlags.NonPublic);
            signalStopped = wrapperType.GetMethod("SignalStopped", BindingFlags.Instance | BindingFlags.NonPublic);
            if (service == null || handleProperty == null || signalStopped == null)
                throw new InvalidOperationException("Unsupported WinSW service status layout");
            serviceStatusHandle = (IntPtr)handleProperty.GetValue(service, null);
            if (serviceStatusHandle == IntPtr.Zero)
                throw new InvalidOperationException("Lifecycle extension must run in a real SCM Service");

            // Subscribe BEFORE spawning the application. The hook handles only
            // the known vendor access-denied branch, synchronously on its exit
            // callback thread. A separate Process.Exited watcher would race the
            // vendor callback disposing Process and is deliberately not used.
            AppDomain.CurrentDomain.FirstChanceException += OnFirstChanceException;
            var raw = CreateJobObjectW(IntPtr.Zero, null);
            if (raw == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(), "Create wrapper containment job");
            var job = new SafeFileHandle(raw, true);
            var limits = new ExtendedLimits();
            limits.Basic.Flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, NO breakaway
            if (!SetInformationJobObject(job, 9, ref limits, (uint)Marshal.SizeOf(typeof(ExtendedLimits))))
            {
                var error = Marshal.GetLastWin32Error(); job.Dispose();
                throw new Win32Exception(error, "Configure wrapper containment job");
            }
            // Final fallible operation before retaining the process-lifetime job.
            // OnWrapperStarted precedes WinSW's CreateProcess, so no spawn/assign gap.
            if (!AssignProcessToJobObject(job, GetCurrentProcess()))
            {
                var error = Marshal.GetLastWin32Error(); job.Dispose();
                throw new Win32Exception(error, "Assign wrapper to containment job (nested job incompatible)");
            }
            lifetimeJob = job;
        }

        private static void OnFirstChanceException(object sender, FirstChanceExceptionEventArgs args)
        {
            var exception = args.Exception;
            var native = exception.InnerException as Win32Exception;
            if (exception.GetType().FullName != "WinSW.CommandException" ||
                native == null || native.NativeErrorCode != 5 || signalStopped == null) return;
            var frames = new StackTrace().GetFrames();
            if (frames == null) return;
            bool knownBranch = false;
            foreach (var frame in frames)
                if (frame.GetMethod() == signalStopped) { knownBranch = true; break; }
            if (!knownBranch || Interlocked.CompareExchange(ref reportingStopped, 1, 0) != 0) return;

            // SignalStopped is reached ONLY after WinSW observed its actual
            // child exit code 0, outside orderly SCM stop. Do not classify other
            // errors or request receipt as process completion. Nonzero crashes
            // and normal SCM stop retain WinSW's original recovery/drain paths.
            var status = new ServiceStatus();
            status.Type = 0x10; // SERVICE_WIN32_OWN_PROCESS
            status.State = 1;   // SERVICE_STOPPED
            if (!SetServiceStatus(serviceStatusHandle, ref status))
            {
                // Leave the original exception/diagnostic intact, never fake a
                // successful stop if SCM refused the owned status handle.
                try { Console.Error.WriteLine("Hamster lifecycle: SetServiceStatus failed: " + Marshal.GetLastWin32Error()); }
                catch { } // diagnostic output must not throw from the CLR hook
                return;
            }
            Environment.Exit(0); // same intended vendor branch, without ALL_ACCESS SCM open
        }

        [StructLayout(LayoutKind.Sequential)] private struct BasicLimits
        {
            public long UserTime, JobTime;
            public uint Flags;
            public UIntPtr MinimumWorkingSet, MaximumWorkingSet;
            public uint ActiveProcessLimit;
            public UIntPtr Affinity;
            public uint Priority, SchedulingClass;
        }
        [StructLayout(LayoutKind.Sequential)] private struct IoCounters
        { public ulong ReadOps, WriteOps, OtherOps, ReadBytes, WriteBytes, OtherBytes; }
        [StructLayout(LayoutKind.Sequential)] private struct ExtendedLimits
        {
            public BasicLimits Basic;
            public IoCounters Io;
            public UIntPtr ProcessMemory, JobMemory, PeakProcessMemory, PeakJobMemory;
        }
        [StructLayout(LayoutKind.Sequential)] private struct ServiceStatus
        { public uint Type, State, AcceptedControls, Win32Exit, ServiceExit, CheckPoint, WaitHint; }
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr CreateJobObjectW(IntPtr attributes, string name);
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetInformationJobObject(SafeFileHandle job, int kind, ref ExtendedLimits limits, uint length);
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool AssignProcessToJobObject(SafeFileHandle job, IntPtr process);
        [DllImport("kernel32.dll")] private static extern IntPtr GetCurrentProcess();
        [DllImport("advapi32.dll", SetLastError = true)]
        private static extern bool SetServiceStatus(IntPtr handle, ref ServiceStatus status);
    }
}
