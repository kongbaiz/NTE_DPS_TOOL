using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
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
                 [--target <package suffix>]... [--all-datatables]
                 [--mapping-filter <text>]... [--file-filter <text>]...

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
    ["paks_directory_name"] = Path.GetFileName(paksDirectory),
    ["game"] = EGame.GAME_NevernessToEverness.ToString(),
    ["usmap_file"] = options.UsmapPath is null
        ? null
        : Path.GetFileName(Path.GetFullPath(options.UsmapPath)),
    ["paths_redacted"] = true
};

if (options.MappingFilters.Count > 0)
{
    if (options.UsmapPath is null)
    {
        Console.Error.WriteLine("--mapping-filter requires --usmap");
        return 2;
    }
    var mappingsProvider = new FileUsmapTypeMappingsProvider(
        Path.GetFullPath(options.UsmapPath),
        StringComparer.OrdinalIgnoreCase);
    var mappings = mappingsProvider.MappingsForGame!;
    report["mapping_types"] = new JArray(
        mappings.Types
            .Where(pair => options.MappingFilters.Any(filter =>
                pair.Key.Contains(filter, StringComparison.OrdinalIgnoreCase)))
            .OrderBy(pair => pair.Key)
            .Select(pair => new JObject
            {
                ["name"] = pair.Key,
                ["super"] = pair.Value.SuperType,
                ["property_count"] = pair.Value.PropertyCount,
                ["properties"] = new JArray(pair.Value.Properties
                    .OrderBy(property => property.Key)
                    .Select(property => new JObject
                    {
                        ["slot"] = property.Key,
                        ["index"] = property.Value.Index,
                        ["name"] = property.Value.Name,
                        ["type"] = property.Value.MappingType.Type,
                        ["struct_type"] = property.Value.MappingType.StructType,
                        ["enum_name"] = property.Value.MappingType.EnumName
                    }))
            }));
    var mappingReportPath = Path.Combine(outputDirectory, "usmap_mappings.json");
    File.WriteAllText(mappingReportPath, report.ToString(Formatting.Indented));
    Console.WriteLine($"Report: {Path.GetFileName(mappingReportPath)}");
    Console.WriteLine(report.ToString(Formatting.Indented));
    return 0;
}

try
{
    using var provider = new DefaultFileProvider(
        paksDirectory,
        options.AllDataTables || options.FileFilters.Count > 0 || options.Targets.Count > 0
            ? SearchOption.AllDirectories
            : SearchOption.TopDirectoryOnly,
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

    var keyText = options.AesKeyFile is not null
        ? File.ReadAllText(options.AesKeyFile).Trim()
        : Environment.GetEnvironmentVariable("NTE_AES_KEY")?.Trim();
    if (!string.IsNullOrWhiteSpace(keyText))
    {
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

    if (options.FileFilters.Count > 0)
    {
        report["file_matches"] = new JArray(
            packageFiles
                .Where(file => options.FileFilters.Any(filter =>
                    file.PathWithoutExtension.Contains(
                        filter,
                        StringComparison.OrdinalIgnoreCase)))
                .GroupBy(
                    file => file.PathWithoutExtension,
                    StringComparer.OrdinalIgnoreCase)
                .Select(group => group.First())
                .OrderBy(file => file.Path)
                .Select(file => new JObject
                {
                    ["path"] = file.Path,
                    ["path_without_extension"] = file.PathWithoutExtension
                }));
    }

    if (options.AllDataTables && !blockedByEncryption)
    {
        var candidates = packageFiles
            .Where(file => file.PathWithoutExtension.Contains(
                "/DataTable/",
                StringComparison.OrdinalIgnoreCase))
            .GroupBy(
                file => file.PathWithoutExtension,
                StringComparer.OrdinalIgnoreCase)
            .Select(group => group.First())
            .OrderBy(file => file.Path)
            .ToArray();
        var exported = 0;
        var skipped = 0;
        var failed = new JArray();
        var outputPaths = new HashSet<string>(StringComparer.OrdinalIgnoreCase);

        foreach (var file in candidates)
        {
            try
            {
                var package = provider.LoadPackage(file);
                var exports = JArray.FromObject(package.GetExports());
                var isDataTable = exports
                    .OfType<JObject>()
                    .Any(export => string.Equals(
                        export.Value<string>("Type"),
                        "DataTable",
                        StringComparison.OrdinalIgnoreCase));
                if (!isDataTable)
                {
                    skipped++;
                    continue;
                }

                var normalizedPath = file.PathWithoutExtension.Replace('\\', '/');
                const string gameContentPrefix = "HT/Content/";
                var relativePath = normalizedPath.StartsWith(
                    gameContentPrefix,
                    StringComparison.OrdinalIgnoreCase)
                    ? normalizedPath[gameContentPrefix.Length..]
                    : normalizedPath.TrimStart('/');
                if (!outputPaths.Add(relativePath))
                {
                    throw new InvalidOperationException(
                        $"Multiple packages map to the same output path: {relativePath}");
                }
                var destination = Path.Combine(
                    outputDirectory,
                    relativePath.Replace('/', Path.DirectorySeparatorChar) + ".json");
                Directory.CreateDirectory(Path.GetDirectoryName(destination)!);
                File.WriteAllText(destination, exports.ToString(Formatting.Indented));
                exported++;
            }
            catch (Exception exception)
            {
                failed.Add(new JObject
                {
                    ["path"] = file.Path,
                    ["error_type"] = exception.GetType().FullName,
                    ["error"] = exception.Message
                });
            }
        }

        report["all_datatables"] = new JObject
        {
            ["candidate_count"] = candidates.Length,
            ["exported_count"] = exported,
            ["unique_output_count"] = outputPaths.Count,
            ["skipped_non_datatable_count"] = skipped,
            ["failed_count"] = failed.Count,
            ["outputs"] = new JArray(outputPaths.OrderBy(path => path)),
            ["failures"] = failed
        };
    }

    if (!options.AllDataTables && options.FileFilters.Count == 0)
    {
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
                result["output"] = normalizedTarget + ".json";
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
Console.WriteLine($"Report: {Path.GetFileName(reportPath)}");
Console.WriteLine(report.ToString(Formatting.Indented));
return report["fatal_error"] is null ? 0 : 1;

static Options? ParseArguments(string[] arguments)
{
    string? paks = null;
    string output = "target/cue4parse-export";
    string? usmap = null;
    string? aesKeyFile = null;
    var allDataTables = false;
    var targets = new List<string>();
    var mappingFilters = new List<string>();
    var fileFilters = new List<string>();

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
                case "--all-datatables":
                    allDataTables = true;
                    break;
                case "--mapping-filter":
                    mappingFilters.Add(NextValue());
                    break;
                case "--file-filter":
                    fileFilters.Add(NextValue());
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

    return paks is null
        ? null
        : new Options(
            paks,
            output,
            usmap,
            aesKeyFile,
            targets,
            allDataTables,
            mappingFilters,
            fileFilters);
}

internal sealed record Options(
    string PaksDirectory,
    string OutputDirectory,
    string? UsmapPath,
    string? AesKeyFile,
    List<string> Targets,
    bool AllDataTables,
    List<string> MappingFilters,
    List<string> FileFilters);
