// 顶替 Desktop 启动的那份 claude.exe：转发全部参数给同目录 claude-real.exe，把 fastMode:true 合并进 --settings。
// Desktop 只比 .verified 不比 exe sha，且 access(exe,X_OK) 通过即可，故可直接顶替。
// 目标 csc v4.0.30319（C# 5）：不用字符串插值、?.、表达式体成员。
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.RegularExpressions;
using System.Web.Script.Serialization;
using Microsoft.Win32;

[assembly: AssemblyInformationalVersion("__CPP_WRAPPER_SOURCE_HASH__")]

class W {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr CreateJobObject(IntPtr attributes, string name);
    [DllImport("kernel32.dll")]
    static extern bool SetInformationJobObject(IntPtr job, int infoClass, ref JOBOBJECT_EXTENDED_LIMIT_INFORMATION info, int length);
    [DllImport("kernel32.dll")]
    static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

    [StructLayout(LayoutKind.Sequential)]
    struct IO_COUNTERS {
        public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
        public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
        public long PerProcessUserTimeLimit, PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize, MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass, SchedulingClass;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public UIntPtr ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryUsed, PeakJobMemoryUsed;
    }
    const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000;
    const int JobObjectExtendedLimitInformation = 9;

    // Desktop 强杀 wrapper 时只终止这一个 PID，Job Object 让 claude-real 一起消失，不留孤儿占着管道
    static void BindToJob(Process child) {
        IntPtr job = CreateJobObject(IntPtr.Zero, null);
        if (job == IntPtr.Zero) return;
        var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if (SetInformationJobObject(job, JobObjectExtendedLimitInformation, ref info, Marshal.SizeOf(info))) {
            AssignProcessToJobObject(job, child.Handle);
        }
    }

    // Windows 命令行参数转义（CommandLineToArgvW 的反向规则）
    internal static string Q(string s) {
        if (s.Length > 0 && s.IndexOfAny(new[] { ' ', '\t', '"', '\n' }) < 0) return s;
        var b = new StringBuilder("\"");
        int bs = 0;
        foreach (char c in s) {
            if (c == '\\') bs++;
            else if (c == '"') { b.Append('\\', bs * 2 + 1); b.Append('"'); bs = 0; }
            else { if (bs > 0) { b.Append('\\', bs); bs = 0; } b.Append(c); }
        }
        b.Append('\\', bs * 2);
        b.Append('"');
        return b.ToString();
    }

    static Dictionary<string, object> ParseSettings(string json) {
        try {
            return new JavaScriptSerializer { MaxJsonLength = 16 * 1024 * 1024 }.DeserializeObject(json) as Dictionary<string, object>;
        } catch {
            return null;
        }
    }

    internal static string MergeFast(string json) {
        var t = (json ?? "").Trim();
        if (t.Length == 0) return "{\"fastMode\":true}";
        var settings = ParseSettings(t);
        if (settings == null || settings.ContainsKey("fastMode")) return json;
        settings["fastMode"] = true;
        return new JavaScriptSerializer { MaxJsonLength = 16 * 1024 * 1024 }.Serialize(settings);
    }

    internal static bool ManagedSettingsRestricted(string json) {
        var settings = ParseSettings(json);
        if (settings == null) return true;
        object value;
        if (settings.TryGetValue("fastMode", out value) && Object.Equals(value, false)) return true;
        if (settings.TryGetValue("fastModePerSessionOptIn", out value) && Object.Equals(value, true)) return true;
        if (settings.ContainsKey("processWrapper")) return true;
        if (settings.TryGetValue("env", out value)) {
            var env = value as Dictionary<string, object>;
            if (env != null && (env.ContainsKey("CLAUDE_CODE_DISABLE_FAST_MODE") || env.ContainsKey("CLAUDE_CODE_PROCESS_WRAPPER"))) return true;
        }
        return false;
    }

    internal static bool ManagedFileRestricted(string path) {
        try { return ManagedSettingsRestricted(File.ReadAllText(path)); }
        catch (FileNotFoundException) { return false; }
        catch (DirectoryNotFoundException) { return false; }
        catch { return true; }
    }

    internal static bool ManagedDirectoryRestricted(string path) {
        try {
            foreach (var file in Directory.GetFiles(path, "*.json")) {
                if (!Path.GetFileName(file).StartsWith(".", StringComparison.Ordinal) && ManagedFileRestricted(file)) return true;
            }
            return false;
        } catch (DirectoryNotFoundException) { return false; }
        catch { return true; }
    }

    internal static bool ManagedRegistryValueRestricted(object value) {
        return value != null && (!(value is string) || ManagedSettingsRestricted((string)value));
    }

    static bool ManagedRegistryRestricted(RegistryHive hive, RegistryView view) {
        try {
            using (var root = RegistryKey.OpenBaseKey(hive, view))
            using (var key = root.OpenSubKey(@"SOFTWARE\Policies\ClaudeCode")) {
                return key != null && ManagedRegistryValueRestricted(key.GetValue("Settings", null, RegistryValueOptions.DoNotExpandEnvironmentNames));
            }
        } catch { return true; }
    }

    static bool ManagedFastRestricted() {
        var root = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), "ClaudeCode");
        if (ManagedFileRestricted(Path.Combine(root, "managed-settings.json")) || ManagedDirectoryRestricted(Path.Combine(root, "managed-settings.d"))) return true;
        foreach (var hive in new[] { RegistryHive.LocalMachine, RegistryHive.CurrentUser }) {
            if (ManagedRegistryRestricted(hive, RegistryView.Registry32)) return true;
            if (Environment.Is64BitOperatingSystem && ManagedRegistryRestricted(hive, RegistryView.Registry64)) return true;
        }
        return false;
    }

    internal static bool DesktopFastOverride(Func<string, string> env, Func<bool> managedRestricted) {
        return env("CLAUDE_CODE_ENTRYPOINT") == "claude-desktop-3p"
            && env("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST") == "1"
            && env("CLAUDE_CODE_DISABLE_FAST_MODE") == "1"
            && env("CLAUDE_CODE_PROCESS_WRAPPER") == null
            && env("CLAUDE_CODE_MANAGED_SETTINGS_PATH") == null
            && !managedRestricted();
    }

    internal static void ApplyFastEnvironment(ProcessStartInfo command, Func<bool> managedRestricted) {
        var env = command.EnvironmentVariables;
        if (!DesktopFastOverride(key => env[key], managedRestricted)) return;
        env.Remove("CLAUDE_CODE_DISABLE_FAST_MODE");
        var token = env["ANTHROPIC_AUTH_TOKEN"];
        if (String.IsNullOrEmpty(env["ANTHROPIC_API_KEY"]) && token != null && token.Length > 24
            && Regex.IsMatch(token, @"\Ask-ant-api[A-Za-z0-9_-]+\z")) {
            env["ANTHROPIC_API_KEY"] = token;
        }
    }

    internal static string MergeArguments(string[] a) {
        var outArgs = new List<string>();
        bool merged = false;
        for (int i = 0; i < a.Length; i++) {
            if (a[i] == "--settings") {
                outArgs.Add("--settings");
                if (i + 1 < a.Length) outArgs.Add(MergeFast(a[++i]));
                merged = true;
            } else if (a[i].StartsWith("--settings=", StringComparison.Ordinal)) {
                outArgs.Add("--settings=" + MergeFast(a[i].Substring("--settings=".Length)));
                merged = true;
            } else {
                outArgs.Add(a[i]);
            }
        }
        if (!merged) {
            outArgs.Add("--settings");
            outArgs.Add("{\"fastMode\":true}");
        }
        var sb = new StringBuilder();
        foreach (var x in outArgs) { sb.Append(Q(x)); sb.Append(' '); }
        return sb.ToString();
    }

    static int Main(string[] a) {
        string dir = Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location);
        string real = Path.Combine(dir, "claude-real.exe");
        if (!File.Exists(real)) {
            Console.Error.WriteLine("[fast-wrapper] claude-real.exe missing");
            return 1;
        }
        // 不 redirect 标准流：子进程继承 stdin/stdout/stderr，透传 SDK 的 stream-json 管道
        var psi = new ProcessStartInfo { FileName = real, Arguments = MergeArguments(a), UseShellExecute = false };
        ApplyFastEnvironment(psi, ManagedFastRestricted);
        var p = Process.Start(psi);
        BindToJob(p);
        p.WaitForExit();
        return p.ExitCode;
    }
}
