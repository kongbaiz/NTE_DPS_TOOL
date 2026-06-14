using CUE4Parse.Encryption.Aes;
using CUE4Parse.FileProvider;
using CUE4Parse.MappingsProvider.Usmap;
using CUE4Parse.UE4.Objects.Core.Misc;
using CUE4Parse.UE4.Versions;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

const string usage = """
Usage:
  Cue4ParseProbe --paks <directory> [--output <directory>]
                 [--usmap <file>] [--aes-key-file <file>]
                 [--target <package suffix>]...

The AES key file must contain one authorized 32-byte hexadecimal key.
The key is never written to logs or reports.
""";

var options = ParseArguments(args);
if (options is null)
{
    Console.Error.WriteLine(usage);
    return 2;
}

var paksDirectory = Path.GetFullPath(options.PaksDirectory);
var outputDirectory = Path.GetFullPath(options.OutputDirectory);
Directory.CreateDirectory(outputDirectory);

var targets = options.Targets.Count > 0
    ? options.Targets
    :
    [
        "DataTable/Character/DT_Character",
        "DataTable/Skill/DT_GameplayEffectMappingData",
        "DataTable/Skill/DT_SkillDamageData",
        "DataTable/Skill/Wooden/DT_WoodenStructData",
        "DataTable/Skill/DT_GameplayAbilityTipsData",
        "DataTable/Reaction/DT_ReactionData",
        "DataTable/Reaction/DT_ReactionDetailUIData",
        "DataTable/Reaction/DT_ReactionDamageData",
        "DataTable/Reaction/DT_ReactionElementTypeData",
        "DataTable/Reaction/DT_ReactionExtendDataTable"
    ];

var report = new JObject
{
    ["generated_at"] = DateTimeOffset.UtcNow.ToString("O"),
    ["paks_directory"] = paksDirectory,
    ["game"] = EGame.GAME_NevernessToEverness.ToString(),
    ["usmap"] = options.UsmapPath is null ? null : Path.GetFullPath(options.UsmapPath)
};

try
{
    using var provider = new DefaultFileProvider(
        paksDirectory,
        SearchOption.TopDirectoryOnly,
        new VersionContainer(EGame.GAME_NevernessToEverness),
        StringComparer.OrdinalIgnoreCase);

    if (options.UsmapPath is not null)
    {
        provider.MappingsContainer = new FileUsmapTypeMappingsProvider(
            Path.GetFullPath(options.UsmapPath),
            StringComparer.OrdinalIgnoreCase);
    }

    provider.Initialize();

    report["registered_archives"] = new JArray(
        provider.UnloadedVfs.Concat(provider.MountedVfs)
            .OrderBy(reader => reader.Name)
            .Select(reader => new JObject
            {
                ["name"] = reader.Name,
                ["encrypted"] = reader.IsEncrypted,
                ["encryption_guid"] = reader.EncryptionKeyGuid.ToString(),
                ["has_directory_index"] = reader.HasDirectoryIndex,
                ["file_count"] = reader.FileCount,
                ["mounted"] = provider.MountedVfs.Contains(reader)
            }));
    report["required_keys_before_submit"] = new JArray(
        provider.RequiredKeys.Select(guid => guid.ToString()));

    if (options.AesKeyFile is not null)
    {
        var keyText = File.ReadAllText(options.AesKeyFile).Trim();
        var key = new FAesKey(keyText);
        foreach (var guid in provider.RequiredKeys.ToArray())
        {
            provider.SubmitKey(guid, key);
        }
    }

    provider.Mount();
    report["mounted_archive_count"] = provider.MountedVfs.Count;
    report["unmounted_archive_count"] = provider.UnloadedVfs.Count;
    report["required_keys_after_submit"] = new JArray(
        provider.RequiredKeys.Select(guid => guid.ToString()));
    report["available_file_count"] = provider.Files.Count;
    var blockedByEncryption = provider.Files.Count == 0 && provider.RequiredKeys.Count > 0;
    report["status"] = blockedByEncryption ? "blocked_by_encryption" : "index_available";
    if (blockedByEncryption)
    {
        report["next_action"] =
            "Provide an authorized 32-byte AES key with --aes-key-file.";
    }

    var packageFiles = provider.Files.Values
        .Where(file => file.IsUePackage)
        .ToArray();
    var targetResults = new JArray();

    foreach (var target in targets)
    {
        var normalizedTarget = target.Replace('\\', '/').TrimStart('/');
        var matches = packageFiles
            .Where(file => file.PathWithoutExtension.EndsWith(
                normalizedTarget,
                StringComparison.OrdinalIgnoreCase))
            .OrderBy(file => file.Path)
            .ToArray();

        var result = new JObject
        {
            ["target"] = normalizedTarget,
            ["matches"] = new JArray(matches.Select(file => file.Path))
        };

        if (blockedByEncryption)
        {
            result["status"] = "blocked_by_encryption";
            targetResults.Add(result);
            continue;
        }

        if (matches.Length == 0)
        {
            result["status"] = "not_found";
            targetResults.Add(result);
            continue;
        }

        var file = matches[0];
        try
        {
            var package = provider.LoadPackage(file);
            var exports = package.GetExports();
            var destination = Path.Combine(
                outputDirectory,
                normalizedTarget.Replace('/', Path.DirectorySeparatorChar) + ".json");
            Directory.CreateDirectory(Path.GetDirectoryName(destination)!);
            File.WriteAllText(
                destination,
                JsonConvert.SerializeObject(exports, Formatting.Indented));
            result["status"] = "exported";
            result["output"] = destination;
            result["export_count"] = exports.Count();
        }
        catch (Exception exception)
        {
            result["status"] = "load_failed";
            result["error_type"] = exception.GetType().FullName;
            result["error"] = exception.Message;
        }

        targetResults.Add(result);
    }

    report["targets"] = targetResults;
}
catch (Exception exception)
{
    report["fatal_error_type"] = exception.GetType().FullName;
    report["fatal_error"] = exception.Message;
}

var reportPath = Path.Combine(outputDirectory, "cue4parse_report.json");
File.WriteAllText(reportPath, report.ToString(Formatting.Indented));
Console.WriteLine($"Report: {reportPath}");
Console.WriteLine(report.ToString(Formatting.Indented));
return report["fatal_error"] is null ? 0 : 1;

static Options? ParseArguments(string[] arguments)
{
    string? paks = null;
    string output = "target/cue4parse-export";
    string? usmap = null;
    string? aesKeyFile = null;
    var targets = new List<string>();

    for (var index = 0; index < arguments.Length; index++)
    {
        string NextValue()
        {
            if (++index >= arguments.Length)
            {
                throw new ArgumentException($"Missing value for {arguments[index - 1]}");
            }

            return arguments[index];
        }

        try
        {
            switch (arguments[index])
            {
                case "--paks":
                    paks = NextValue();
                    break;
                case "--output":
                    output = NextValue();
                    break;
                case "--usmap":
                    usmap = NextValue();
                    break;
                case "--aes-key-file":
                    aesKeyFile = NextValue();
                    break;
                case "--target":
                    targets.Add(NextValue());
                    break;
                default:
                    return null;
            }
        }
        catch (ArgumentException)
        {
            return null;
        }
    }

    return paks is null ? null : new Options(paks, output, usmap, aesKeyFile, targets);
}

internal sealed record Options(
    string PaksDirectory,
    string OutputDirectory,
    string? UsmapPath,
    string? AesKeyFile,
    List<string> Targets);
