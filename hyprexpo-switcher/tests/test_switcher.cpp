#include "../src/WorkspaceModel.hpp"
#include "../src/workspace_utils.hpp"
#include "../../hyprexpo/common.hpp"

#include <QtTest/QtTest>
#include <sys/un.h>

class SwitcherTests : public QObject {
    Q_OBJECT

  private slots:
    void parsesHelloCommand();
    void parsesWatchCommand();
    void parsesRefreshCommand();
    void parsesPingCommand();
    void parsesSwitcherShowForwardCommand();
    void parsesSwitcherShowReverseCommand();
    void parsesSwitcherHideCommand();
    void rejectsInvalidWatchCommand();
    void dedupesWorkspaceIds();
    void computesLandscapePreviewSize();
    void computesPortraitPreviewSize();
    void rejectsInvalidPreviewSize();
    void buildsRuntimePaths();
    void buildsSwitcherSocketPath();
    void buildsWorkspaceDescriptors();
    void filtersEmptyWorkspaces();
    void sortsActiveWorkspaceFirst();
    void sortsByMRUOrder();
    void computesInitialSelection();
    void fallsBackToClassWhenTitleMissing();
    void keepsSocketPathShortEnough();
    void modelCyclesForward();
    void modelCyclesBackward();
    void modelBootstrapsPreview();
    void modelUpdatesPreview();
    void formatsHelloEvent();
    void formatsPreviewEvent();
};

void SwitcherTests::parsesHelloCommand() {
    std::string error;
    const auto  parsed = hyprexpo::parseClientCommand("HELLO", error);
    QVERIFY(parsed.has_value());
    QCOMPARE(parsed->command, hyprexpo::eClientCommand::HELLO);
}

void SwitcherTests::parsesWatchCommand() {
    std::string error;
    const auto  parsed = hyprexpo::parseClientCommand("WATCH 2 4 8", error);
    QVERIFY(parsed.has_value());
    QCOMPARE(parsed->workspaceIDs, std::vector<int>({2, 4, 8}));
}

void SwitcherTests::parsesRefreshCommand() {
    std::string error;
    const auto  parsed = hyprexpo::parseClientCommand("REFRESH 2 2 8", error);
    QVERIFY(parsed.has_value());
    QCOMPARE(parsed->command, hyprexpo::eClientCommand::REFRESH);
    QCOMPARE(parsed->workspaceIDs, std::vector<int>({2, 8}));
}

void SwitcherTests::parsesPingCommand() {
    std::string error;
    const auto  parsed = hyprexpo::parseClientCommand("PING", error);
    QVERIFY(parsed.has_value());
    QCOMPARE(parsed->command, hyprexpo::eClientCommand::PING);
    QVERIFY(parsed->workspaceIDs.empty());
}

void SwitcherTests::parsesSwitcherShowForwardCommand() {
    std::string error;
    const auto  parsed = hyprexpo::parseSwitcherCommand("SHOW FORWARD", error);
    QVERIFY(parsed.has_value());
    QCOMPARE(parsed->command, hyprexpo::eSwitcherCommand::SHOW_FORWARD);
}

void SwitcherTests::parsesSwitcherShowReverseCommand() {
    std::string error;
    const auto  parsed = hyprexpo::parseSwitcherCommand("SHOW REVERSE", error);
    QVERIFY(parsed.has_value());
    QCOMPARE(parsed->command, hyprexpo::eSwitcherCommand::SHOW_REVERSE);
}

void SwitcherTests::parsesSwitcherHideCommand() {
    std::string error;
    const auto  parsed = hyprexpo::parseSwitcherCommand("HIDE", error);
    QVERIFY(parsed.has_value());
    QCOMPARE(parsed->command, hyprexpo::eSwitcherCommand::HIDE);
}

void SwitcherTests::rejectsInvalidWatchCommand() {
    std::string error;
    const auto  parsed = hyprexpo::parseClientCommand("WATCH nope", error);
    QVERIFY(!parsed.has_value());
    QVERIFY(!error.empty());
}

void SwitcherTests::dedupesWorkspaceIds() {
    const auto deduped = hyprexpo::dedupeWorkspaceIDs({2, 2, 5, 5, 7});
    QCOMPARE(deduped, std::vector<int>({2, 5, 7}));
}

void SwitcherTests::computesLandscapePreviewSize() {
    const auto size = hyprexpo::computePreviewSize(2560, 1440, 480);
    QCOMPARE(size.width, 853);
    QCOMPARE(size.height, 480);
}

void SwitcherTests::computesPortraitPreviewSize() {
    const auto size = hyprexpo::computePreviewSize(1440, 2560, 480);
    QCOMPARE(size.width, 480);
    QCOMPARE(size.height, 853);
}

void SwitcherTests::rejectsInvalidPreviewSize() {
    const auto size = hyprexpo::computePreviewSize(0, 1440, 480);
    QCOMPARE(size.width, 0);
    QCOMPARE(size.height, 0);
}

void SwitcherTests::buildsRuntimePaths() {
    const auto runtimeDir  = hyprexpo::runtimeDirectory("/run/user/1000", "abc123");
    const auto socketPath  = hyprexpo::socketPath("/run/user/1000", "abc123");
    const auto previewPath = hyprexpo::previewPath("/run/user/1000", "abc123", 7);

    QVERIFY(QString::fromStdString(runtimeDir.string()).startsWith(QStringLiteral("/run/user/1000/hx/")));
    QVERIFY(!QString::fromStdString(runtimeDir.string()).contains(QStringLiteral("abc123")));
    QCOMPARE(QString::fromStdString(socketPath.string()), QString::fromStdString((runtimeDir / "preview.sock").string()));
    QVERIFY(QString::fromStdString(socketPath.string()).size() < 108);
    QCOMPARE(QString::fromStdString(previewPath.string()), QString::fromStdString((runtimeDir / "workspace-7.jpg").string()));
}

void SwitcherTests::buildsSwitcherSocketPath() {
    const auto runtimeDir   = hyprexpo::runtimeDirectory("/run/user/1000", "abc123");
    const auto switcherPath = hyprexpo::switcherSocketPath("/run/user/1000", "abc123");

    QCOMPARE(QString::fromStdString(switcherPath.string()), QString::fromStdString((runtimeDir / "switcher.sock").string()));
    QVERIFY(QString::fromStdString(switcherPath.string()).size() < 108);
}

void SwitcherTests::buildsWorkspaceDescriptors() {
    const QByteArray monitors = R"([{"focused":true,"activeWorkspace":{"id":3}}])";
    const QByteArray workspaces =
        R"([{"id":2,"name":"2","windows":1,"lastwindowtitle":"Editor"},{"id":3,"name":"3","windows":1,"lastwindowtitle":"Browser"},{"id":9,"name":"9","windows":1,"lastwindowtitle":""}])";
    const QByteArray clients =
        R"([{"mapped":true,"workspace":{"id":2},"title":"Editor","class":"code","focusHistoryID":4},{"mapped":true,"workspace":{"id":3},"title":"Browser","class":"zen","focusHistoryID":0},{"mapped":true,"workspace":{"id":9},"title":"","class":"ghostty","focusHistoryID":9}])";

    const auto result = buildWorkspaceDescriptors(monitors, workspaces, clients);
    QCOMPARE(result.size(), 3);
    QCOMPARE(result[0].id, 3);
    QVERIFY(result[0].active);
    QCOMPARE(result[2].subtitle, QStringLiteral("ghostty"));
    QCOMPARE(result[0].appClass, QStringLiteral("zen"));
    QCOMPARE(result[0].windowCount, 1);
}

void SwitcherTests::filtersEmptyWorkspaces() {
    const QByteArray monitors = R"([{"focused":true,"activeWorkspace":{"id":2}}])";
    const QByteArray workspaces =
        R"([{"id":2,"name":"2","windows":1,"lastwindowtitle":"Editor"},{"id":3,"name":"3","windows":0,"lastwindowtitle":""},{"id":9,"name":"9","windows":2,"lastwindowtitle":"Chat"}])";
    const QByteArray clients =
        R"([{"mapped":true,"workspace":{"id":2},"title":"Editor","class":"code","focusHistoryID":1},{"mapped":true,"workspace":{"id":9},"title":"Chat","class":"discord","focusHistoryID":2},{"mapped":true,"workspace":{"id":9},"title":"Browser","class":"zen","focusHistoryID":3}])";

    const auto result = buildWorkspaceDescriptors(monitors, workspaces, clients);
    QCOMPARE(result.size(), 2);
    QCOMPARE(result[0].id, 2);
    QCOMPARE(result[1].id, 9);
}

void SwitcherTests::sortsActiveWorkspaceFirst() {
    const QByteArray monitors = R"([{"focused":true,"activeWorkspace":{"id":20}}])";
    const QByteArray workspaces =
        R"([{"id":30,"name":"30","windows":1,"lastwindowtitle":"c"},{"id":2,"name":"2","windows":1,"lastwindowtitle":"a"},{"id":20,"name":"20","windows":1,"lastwindowtitle":"b"}])";
    const QByteArray clients =
        R"([{"mapped":true,"workspace":{"id":30},"title":"c","class":"gamma","focusHistoryID":7},{"mapped":true,"workspace":{"id":2},"title":"a","class":"alpha","focusHistoryID":2},{"mapped":true,"workspace":{"id":20},"title":"b","class":"beta","focusHistoryID":4}])";

    const auto result = buildWorkspaceDescriptors(monitors, workspaces, clients);
    QCOMPARE(result[0].id, 20);
    QCOMPARE(result[1].id, 2);
    QCOMPARE(result[2].id, 30);
}

void SwitcherTests::sortsByMRUOrder() {
    QVector<SWorkspaceDescriptor> items{
        {.id = 2, .name = "2", .subtitle = "a", .focusHistoryRank = 4, .active = false},
        {.id = 4, .name = "4", .subtitle = "b", .focusHistoryRank = 2, .active = true},
        {.id = 9, .name = "9", .subtitle = "c", .focusHistoryRank = 7, .active = false},
        {.id = 10, .name = "10", .subtitle = "d", .focusHistoryRank = 3, .active = false},
    };

    sortWorkspacesForSwitcher(items, {9, 2});
    QCOMPARE(items[0].id, 4);
    QCOMPARE(items[1].id, 9);
    QCOMPARE(items[2].id, 2);
    QCOMPARE(items[3].id, 10);
}

void SwitcherTests::computesInitialSelection() {
    QVector<SWorkspaceDescriptor> items{
        {.id = 2, .name = "2", .subtitle = "Current", .active = true},
        {.id = 4, .name = "4", .subtitle = "Other", .active = false},
        {.id = 9, .name = "9", .subtitle = "Other", .active = false},
    };

    QCOMPARE(initialSelectionIndex(items, false), 1);
    QCOMPARE(initialSelectionIndex(items, true), 2);
}

void SwitcherTests::fallsBackToClassWhenTitleMissing() {
    const QByteArray monitors = R"([{"focused":true,"activeWorkspace":{"id":4}}])";
    const QByteArray workspaces = R"([{"id":4,"name":"4","windows":1,"lastwindowtitle":""}])";
    const QByteArray clients = R"([{"mapped":true,"workspace":{"id":4},"title":"","class":"ghostty","focusHistoryID":0}])";

    const auto result = buildWorkspaceDescriptors(monitors, workspaces, clients);
    QCOMPARE(result.size(), 1);
    QCOMPARE(result[0].subtitle, QStringLiteral("ghostty"));
    QCOMPARE(result[0].appClass, QStringLiteral("ghostty"));
}

void SwitcherTests::keepsSocketPathShortEnough() {
    const auto path = hyprexpo::socketPath("/run/user/1000", "59f9f2688ac508a0584d1462151195a6c4992f99_1774727667_1360953428");
    QVERIFY(path.string().size() < sizeof(sockaddr_un{}.sun_path));
}

void SwitcherTests::modelCyclesForward() {
    WorkspaceModel model;
    model.setWorkspaces({
        {.id = 2, .name = "2", .subtitle = "Current", .active = true},
        {.id = 4, .name = "4", .subtitle = "Other", .active = false},
        {.id = 9, .name = "9", .subtitle = "Other", .active = false},
    });

    model.setCurrentIndex(1);
    QCOMPARE(model.currentWorkspaceID(), 4);
    model.selectNext();
    QCOMPARE(model.currentWorkspaceID(), 9);
    model.selectNext();
    QCOMPARE(model.currentWorkspaceID(), 2);
}

void SwitcherTests::modelCyclesBackward() {
    WorkspaceModel model;
    model.setWorkspaces({
        {.id = 2, .name = "2", .subtitle = "Current", .active = true},
        {.id = 4, .name = "4", .subtitle = "Other", .active = false},
        {.id = 9, .name = "9", .subtitle = "Other", .active = false},
    });

    model.setCurrentIndex(0);
    model.selectPrevious();
    QCOMPARE(model.currentWorkspaceID(), 9);
}

void SwitcherTests::modelBootstrapsPreview() {
    WorkspaceModel model;
    model.setWorkspaces({
        {.id = 2, .name = "2", .subtitle = "Current", .appClass = "ghostty", .windowCount = 2, .active = true},
    });

    model.bootstrapPreview(2, QStringLiteral("/tmp/workspace-2.jpg"));
    const auto index = model.index(0, 0);
    QCOMPARE(index.data(WorkspaceModel::GenerationRole).toULongLong(), 0ULL);
    QVERIFY(index.data(WorkspaceModel::PreviewPathRole).toUrl().toLocalFile().endsWith(QStringLiteral("workspace-2.jpg")));
    QCOMPARE(index.data(WorkspaceModel::PreviewPathRole).toUrl().query(), QStringLiteral("g=0"));
}

void SwitcherTests::modelUpdatesPreview() {
    WorkspaceModel model;
    model.setWorkspaces({
        {.id = 2, .name = "2", .subtitle = "Current", .appClass = "ghostty", .windowCount = 2, .active = true},
    });

    model.updatePreview(2, QStringLiteral("/tmp/workspace-2.jpg"), 7);
    const auto index = model.index(0, 0);
    QCOMPARE(index.data(WorkspaceModel::GenerationRole).toULongLong(), 7ULL);
    QVERIFY(index.data(WorkspaceModel::PreviewPathRole).toUrl().toLocalFile().endsWith(QStringLiteral("workspace-2.jpg")));
    QCOMPARE(index.data(WorkspaceModel::PreviewPathRole).toUrl().query(), QStringLiteral("g=7"));
    QCOMPARE(index.data(WorkspaceModel::AppClassRole).toString(), QStringLiteral("ghostty"));
    QCOMPARE(index.data(WorkspaceModel::WindowCountRole).toInt(), 2);
}

void SwitcherTests::formatsHelloEvent() {
    const auto payload = hyprexpo::formatHelloEvent("/tmp/preview.sock", 480);
    QVERIFY(payload.starts_with("{\"event\":\"hello\""));
    QVERIFY(payload.find("\"socket\":\"/tmp/preview.sock\"") != std::string::npos);
    QVERIFY(payload.find("\"previewHeight\":480") != std::string::npos);
}

void SwitcherTests::formatsPreviewEvent() {
    const auto payload = hyprexpo::formatPreviewEvent(5, "/tmp/workspace-5.jpg", 853, 480, 9);
    QVERIFY(payload.starts_with("{\"event\":\"preview\""));
    QVERIFY(payload.find("\"workspaceId\":5") != std::string::npos);
    QVERIFY(payload.find("\"generation\":9") != std::string::npos);
}

QTEST_MAIN(SwitcherTests)

#include "test_switcher.moc"
