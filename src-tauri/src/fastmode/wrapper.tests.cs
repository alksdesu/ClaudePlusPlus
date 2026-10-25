using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Reflection;
using System.Text;
using System.Web.Script.Serialization;

class WrapperTests {
    const string Token = "sk-ant-api03-synthetic-test-credential-not-real";
    static int assertions;

    static void Check(bool condition, string message) {
        assertions++;
        if (!condition) throw new Exception(message);
    }

    static Dictionary<string, object> Json(string text) {
        return new JavaScriptSerializer().DeserializeObject(text) as Dictionary<string, object>;
    }

    static ProcessStartInfo Command(string file, string[] args) {
        var text = new StringBuilder();
        foreach (var arg in args) { text.Append(W.Q(arg)); text.Append(' '); }
        var command = new ProcessStartInfo(file, text.ToString()) {
            UseShellExecute = false, RedirectStandardOutput = true, RedirectStandardError = true
        };
        foreach (var key in new[] { "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_PROCESS_WRAPPER", "CLAUDE_CODE_MANAGED_SETTINGS_PATH" }) command.EnvironmentVariables.Remove(key);
        command.EnvironmentVariables["CLAUDE_CODE_ENTRYPOINT"] = "claude-desktop-3p";
        command.EnvironmentVariables["CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"] = "1";
        command.EnvironmentVariables["CLAUDE_CODE_DISABLE_FAST_MODE"] = "1";
        command.EnvironmentVariables["ANTHROPIC_AUTH_TOKEN"] = Token;
        return command;
    }

    static Dictionary<string, object> Run(ProcessStartInfo command, int exit) {
        using (var child = Process.Start(command)) {
            var text = child.StandardOutput.ReadToEnd();
            var error = child.StandardError.ReadToEnd();
            if (!child.WaitForExit(10000)) { child.Kill(); throw new Exception("child timed out"); }
            Check(child.ExitCode == exit, "child exit code mismatch: " + error);
            return Json(text);
        }
    }

    static int Probe(string[] args) {
        object settings = null;
        for (int i = 0; i < args.Length; i++) {
            if (args[i] == "--settings" && i + 1 < args.Length) settings = Json(args[i + 1]);
            if (args[i].StartsWith("--settings=", StringComparison.Ordinal)) settings = Json(args[i].Substring(11));
        }
        var key = Environment.GetEnvironmentVariable("ANTHROPIC_API_KEY");
        Console.WriteLine(new JavaScriptSerializer().Serialize(new {
            args = args, settings = settings,
            disabled = Environment.GetEnvironmentVariable("CLAUDE_CODE_DISABLE_FAST_MODE"),
            tokenRetained = Environment.GetEnvironmentVariable("ANTHROPIC_AUTH_TOKEN") == Token,
            apiKeyPresent = !String.IsNullOrEmpty(key), apiKeyMatchesToken = key == Token,
            apiKeyPreserved = key == "existing-test-key"
        }));
        return Array.IndexOf(args, "--exit-seven") >= 0 ? 7 : 0;
    }

    static void VerifySettings() {
        Check(Object.Equals(Json(W.MergeFast("{}"))["fastMode"], true), "empty settings should default on");
        var off = W.MergeFast("{\"fastMode\":false,\"enabledPlugins\":{\"a\":true}}");
        Check(Object.Equals(Json(off)["fastMode"], false), "explicit off must survive");
        Check(Json(off).ContainsKey("enabledPlugins"), "plugin settings must survive");
        var nested = Json(W.MergeFast("{\"nested\":{\"fastMode\":false},\"disableAutoMode\":\"disable\"}"));
        Check(Object.Equals(nested["fastMode"], true), "nested setting is not the root choice");
        Check(Object.Equals(((Dictionary<string, object>)nested["nested"])["fastMode"], false), "nested setting was rewritten");
        foreach (var input in new[] { "not json", "C:\\settings file.json", "[]" }) Check(W.MergeFast(input) == input, "unknown settings must be preserved");
    }

    static void VerifyRestrictions(string root) {
        foreach (var value in new[] {
            "{\"fastMode\":false}", "{\"fastModePerSessionOptIn\":true}", "{\"processWrapper\":\"policy\"}",
            "{\"env\":{\"CLAUDE_CODE_DISABLE_FAST_MODE\":\"0\"}}", "{\"env\":{\"CLAUDE_CODE_PROCESS_WRAPPER\":\"policy\"}}", "[]", "invalid"
        }) Check(W.ManagedSettingsRestricted(value), "managed restriction was ignored");
        Check(!W.ManagedSettingsRestricted("{\"fastMode\":true,\"permissions\":{}}"), "unrelated policy should not force off");
        Check(!W.ManagedRegistryValueRestricted(null), "absent registry value");
        Check(W.ManagedRegistryValueRestricted(1), "malformed registry value");
        Check(W.ManagedRegistryValueRestricted("{\"fastMode\":false}"), "registry fast policy");
        var policy = Path.Combine(root, "policy.json");
        Check(!W.ManagedFileRestricted(policy), "missing policy file");
        File.WriteAllText(policy, "invalid");
        Check(W.ManagedFileRestricted(policy), "corrupt policy file");
        var dropins = Path.Combine(root, "managed-settings.d");
        Directory.CreateDirectory(dropins);
        File.WriteAllText(Path.Combine(dropins, ".ignored.json"), "invalid");
        Check(!W.ManagedDirectoryRestricted(dropins), "hidden drop-in should be ignored");
        File.WriteAllText(Path.Combine(dropins, "10-fast.json"), "{\"fastMode\":false}");
        Check(W.ManagedDirectoryRestricted(dropins), "managed drop-in was ignored");
        var command = Command("unused", new string[0]);
        W.ApplyFastEnvironment(command, () => true);
        Check(command.EnvironmentVariables["CLAUDE_CODE_DISABLE_FAST_MODE"] == "1", "managed disable flag was removed");
        Check(command.EnvironmentVariables["ANTHROPIC_API_KEY"] == null, "managed credentials were altered");
        foreach (var variable in new[] { "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "CLAUDE_CODE_DISABLE_FAST_MODE", "CLAUDE_CODE_PROCESS_WRAPPER", "CLAUDE_CODE_MANAGED_SETTINGS_PATH" }) {
            command = Command("unused", new string[0]);
            command.EnvironmentVariables[variable] = "other";
            Check(!W.DesktopFastOverride(key => command.EnvironmentVariables[key], () => false), "unexpected launcher accepted: " + variable);
        }
    }

    static void VerifyChild(string root) {
        var wrapper = Path.Combine(root, "claude.exe");
        var args = new[] { "--probe", "folder with spaces\\", "quote\"value", "--settings", "{\"disableAutoMode\":\"disable\",\"enabledPlugins\":{}}" };
        var result = Run(Command(wrapper, args), 0);
        Check(result["disabled"] == null, "real child still has the Desktop default disable flag");
        Check(Object.Equals(result["apiKeyMatchesToken"], true), "real child has no preflight key alias");
        Check(Object.Equals(result["tokenRetained"], true), "original authentication token changed");
        var received = (object[])result["args"];
        Check(Object.Equals(received[1], args[1]) && Object.Equals(received[2], args[2]), "argument quoting changed");
        Check(Object.Equals(((Dictionary<string, object>)result["settings"])["fastMode"], true), "default fast setting missing");
        result = Run(Command(wrapper, new[] { "--probe", "--settings={\"fastMode\":false}" }), 0);
        Check(Object.Equals(((Dictionary<string, object>)result["settings"])["fastMode"], false), "explicit off lost in child");
        var command = Command(wrapper, new[] { "--probe", "--exit-seven" });
        command.EnvironmentVariables["ANTHROPIC_API_KEY"] = "existing-test-key";
        result = Run(command, 7);
        Check(Object.Equals(result["apiKeyPreserved"], true), "existing API key replaced");
        command = Command(wrapper, new[] { "--probe" });
        command.EnvironmentVariables["CLAUDE_CODE_ENTRYPOINT"] = "cli";
        result = Run(command, 0);
        Check(Object.Equals(result["disabled"], "1") && Object.Equals(result["apiKeyPresent"], false), "non-Desktop environment changed");
        command = Command(wrapper, new[] { "--probe" });
        command.EnvironmentVariables["ANTHROPIC_AUTH_TOKEN"] = "gateway-opaque-token";
        result = Run(command, 0);
        Check(Object.Equals(result["apiKeyPresent"], false), "opaque token treated as an Anthropic API key");
    }

    static int Main(string[] args) {
        if (args.Length > 0 && args[0] == "--probe") return Probe(args);
        try {
            var root = Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location);
            VerifySettings();
            VerifyRestrictions(root);
            VerifyChild(root);
            Console.WriteLine("Windows wrapper: " + assertions + " assertions passed");
            return 0;
        } catch (Exception error) {
            Console.Error.WriteLine(error);
            return 1;
        }
    }
}
