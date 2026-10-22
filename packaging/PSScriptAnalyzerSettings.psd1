# PSScriptAnalyzer settings for install.ps1 and packaging/scripts/*.ps1; CI runs
#   Invoke-ScriptAnalyzer -Path <file> -Settings packaging/PSScriptAnalyzerSettings.psd1
@{
    # Information only adds PSAvoidUsingPositionalParameters for the scripts' own helpers.
    Severity     = @('Error', 'Warning')
    # install.ps1 prints progress for a person at a console, often through `irm | iex`,
    # where Write-Output would mix messages into the pipeline's return values.
    ExcludeRules = @('PSAvoidUsingWriteHost')
}
