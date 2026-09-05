// 顶替版本目录的 claude.exe：转发全部参数给同目录 claude-real.exe，把 fastMode:true 合并进 --settings。
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
    static string Q(string s) {
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

    static readonly Regex FastKey = new Regex("\"fastMode\"\\s*:\\s*(true|false)");

    // 保留 Desktop 自带的 disableAutoMode/enabledPlugins；若它已显式传了 fastMode 则就地改写，前插会留下重复 key 让后者胜出
    static string MergeFast(string json) {
        var t = (json ?? "").Trim();
        if (t.Length == 0 || t == "{}") return "{\"fastMode\":true}";
        if (FastKey.IsMatch(t)) return FastKey.Replace(t, "\"fastMode\":true", 1);
        if (t[0] == '{') return "{\"fastMode\":true," + t.Substring(1);
        return "{\"fastMode\":true}";
    }

    static int Main(string[] a) {
        string dir = Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location);
        string real = Path.Combine(dir, "claude-real.exe");
        if (!File.Exists(real)) {
            Console.Error.WriteLine("[fast-wrapper] claude-real.exe missing");
            return 1;
        }
        var outArgs = new List<string>();
        bool merged = false;
        for (int i = 0; i < a.Length; i++) {
            if (a[i] == "--settings" && i + 1 < a.Length) {
                outArgs.Add("--settings");
                outArgs.Add(MergeFast(a[i + 1]));
                i++;
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
        // 不 redirect 标准流：子进程继承 stdin/stdout/stderr，透传 SDK 的 stream-json 管道
        var psi = new ProcessStartInfo { FileName = real, Arguments = sb.ToString(), UseShellExecute = false };
        var p = Process.Start(psi);
        BindToJob(p);
        p.WaitForExit();
        return p.ExitCode;
    }
}
