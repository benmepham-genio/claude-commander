import 'package:claude_commander_client/server_config.dart';
import 'package:claude_commander_client/src/rust/api/mirrors.dart';
import 'package:claude_commander_client/src/rust/api/review.dart';
import 'package:uuid/uuid.dart';

/// Test data builders for the frb mirror/DTO types. They carry required fields
/// with plausible defaults so tests only pass what they assert on.

const testConfig = ServerConfig(
  id: 'test-server',
  name: 'test',
  baseUrl: 'http://127.0.0.1:7878',
  token: 'test-token',
);

/// A stand-in server handle for pages under test — the fake ignores its value.
const testHandle = 'test-handle';

SessionInfo sessionInfo({
  String id = '11111111-2222-3333-4444-555555555555',
  String title = 'Test session',
  String branch = 'test-branch',
  SessionStatus status = SessionStatus.running,
  String program = 'bash',
  String? pendingProgram,
  String projectName = 'my-repo',

  /// The owning project, defaulting to one derived from [id] — i.e. every
  /// session is its own project unless a test says otherwise. Pass the same
  /// value to two sessions to put them in one group, which is what the list's
  /// grouping keys on; sharing only [projectName] is not enough.
  String? projectId,
  int? prNumber,
  PrState prState = PrState.open,
  bool unread = false,
  SessionId? stackParentSessionId,
  DateTime? lastAttachedAt,
  DateTime? createdAt,
  bool keepAlive = false,
  String? currentSection,
  String? sectionOverride,
}) {
  final uuid = UuidValue.fromString(id);
  return SessionInfo(
    id: id,
    sessionId: SessionId(field0: uuid),
    title: title,
    branch: branch,
    status: status,
    program: program,
    pendingProgram: pendingProgram,
    projectId: ProjectId(
      field0: projectId == null ? uuid : UuidValue.fromString(projectId),
    ),
    projectName: projectName,
    prNumber: prNumber,
    prUrl: null,
    prState: prState,
    prDraft: false,
    prLabels: const [],
    reviewDecision: null,
    prReviewers: const [],
    createdAt: createdAt ?? DateTime.utc(2026, 1, 1),
    unread: unread,
    stackParentSessionId: stackParentSessionId,
    prBaseBranch: null,
    prMerged: false,
    currentSection: currentSection,
    sectionOverride: sectionOverride,
    enteredSectionAt: null,
    lastAttachedAt: lastAttachedAt,
    worktreePath: '/tmp/test-worktree',
    tmuxSessionName: 'cc-test',
    keepAlive: keepAlive,
  );
}

ProjectInfoDto projectInfo({
  String id = 'aaaaaaaa-2222-3333-4444-555555555555',
  String name = 'my-repo',
  String repoPath = '/srv/repos/my-repo',
  String mainBranch = 'main',

  /// The repo's `origin` remote. Defaults to null — a project registered from a
  /// local path with no remote — because that is the shape the repo picker's
  /// "already added" badge must NOT treat as a match against anything.
  String? originUrl,

  /// The workspace the project is tagged with; null (the default) is Main.
  String? workspace,
}) => ProjectInfoDto(
  id: ProjectId(field0: UuidValue.fromString(id)),
  name: name,
  repoPath: repoPath,
  mainBranch: mainBranch,
  sessionIds: const [],
  originUrl: originUrl,
  workspace: workspace,
);

/// A clone job as a frontend polls it. Defaults to `Running`, the status
/// `startClone` essentially always answers with.
CloneJobDto cloneJob({
  String id = 'cccccccc-2222-3333-4444-555555555555',
  String sourceLabel = 'acme/widget',
  String dest = '/srv/projects/widget',
  CloneStatusDto status = const CloneStatusDto(
    kind: CloneStatusKind.running,
    message: '',
    isGitRepo: false,
  ),
}) => CloneJobDto(
  id: CloneJobId(field0: UuidValue.fromString(id)),
  sourceLabel: sourceLabel,
  dest: dest,
  status: status,
);

/// A picker row. [cloneUrl] defaults to the https spelling GitHub's API reports
/// and [sshUrl] to the scp spelling `gh` clones with, so a test can pit one
/// against the other the way the badge has to.
GithubRepo githubRepo({
  required String owner,
  required String name,
  String? cloneUrl,
  String? sshUrl,
  String? description,
  bool private = false,
  bool fork = false,
  bool archived = false,
  String defaultBranch = 'main',
  DateTime? pushedAt,
}) => GithubRepo(
  fullName: '$owner/$name',
  owner: owner,
  name: name,
  description: description,
  private: private,
  fork: fork,
  archived: archived,
  defaultBranch: defaultBranch,
  cloneUrl: cloneUrl ?? 'https://github.com/$owner/$name.git',
  sshUrl: sshUrl ?? 'git@github.com:$owner/$name.git',
  pushedAt: pushedAt,
);

HostedRepository hostedRepo({
  required String namespace,
  required String name,
  String? cloneUrl,
  String? sshUrl,
  String? description,
  RepositoryVisibility visibility = RepositoryVisibility.public,
  bool fork = false,
  bool archived = false,
  String? defaultBranch = 'main',
  DateTime? activityAt,
  String hostname = 'gitlab.com',
}) => HostedRepository(
  fullName: '$namespace/$name',
  namespace: namespace,
  name: name,
  description: description,
  visibility: visibility,
  fork: fork,
  archived: archived,
  defaultBranch: defaultBranch,
  cloneUrl: cloneUrl ?? 'https://$hostname/$namespace/$name.git',
  sshUrl: sshUrl ?? 'git@$hostname:$namespace/$name.git',
  activityAt: activityAt,
);

SessionDetail sessionDetail({
  SessionInfo? info,
  AgentState agentState = AgentState.idle,
  String? diffStat = '2 files changed',
  String? paneContent = 'pane snapshot',
}) => SessionDetail(
  info: info ?? sessionInfo(),
  agentState: agentState,
  diffStat: diffStat,
  paneContent: paneContent,
);

ReviewLineDto line(
  ReviewLineOrigin origin,
  String content, {
  int? oldLineno,
  int? newLineno,
}) => ReviewLineDto(
  origin: origin,
  oldLineno: oldLineno,
  newLineno: newLineno,
  content: content,
);

ReviewHunkDto hunk({
  int oldStart = 1,
  int oldLines = 1,
  int newStart = 1,
  int newLines = 2,
  String header = '',
  List<ReviewLineDto>? lines,
}) => ReviewHunkDto(
  oldStart: oldStart,
  oldLines: oldLines,
  newStart: newStart,
  newLines: newLines,
  header: header,
  lines:
      lines ??
      [
        line(
          ReviewLineOrigin.context,
          'context line',
          oldLineno: 1,
          newLineno: 1,
        ),
        line(ReviewLineOrigin.addition, 'added line', newLineno: 2),
      ],
);

ReviewFileDto reviewFile({
  String displayPath = 'src/main.rs',
  ReviewFileStatus status = ReviewFileStatus.modified,
  int added = 1,
  int removed = 0,
  List<ReviewHunkDto>? hunks,
  bool isBinary = false,
  String? binaryMime,
}) => ReviewFileDto(
  displayPath: displayPath,
  oldPath: displayPath,
  newPath: displayPath,
  status: status,
  added: added,
  removed: removed,
  hunks: hunks ?? [hunk()],
  isBinary: isBinary,
  binaryMime: binaryMime,
);

CommentDto comment({
  String id = 'comment-1',
  String file = 'src/main.rs',
  ReviewCommentSide side = ReviewCommentSide.new_,
  int lineStart = 2,
  int lineEnd = 2,
  String snippet = 'added line',
  String text = 'Please fix this',
  ReviewCommentStatus status = ReviewCommentStatus.staged,
}) => CommentDto(
  id: id,
  file: file,
  side: side,
  lineStart: lineStart,
  lineEnd: lineEnd,
  snippet: snippet,
  comment: text,
  status: status,
  createdAt: DateTime.utc(2026, 1, 1),
);

ReviewSnapshotDto reviewSnapshot({
  String base = 'main',
  String contentHash = '42',
  List<ReviewFileDto>? files,
  List<CommentDto>? comments,
  List<String>? reviewed,
}) => ReviewSnapshotDto(
  base: base,
  contentHash: contentHash,
  files: files ?? [reviewFile()],
  comments: comments ?? const [],
  reviewed: reviewed ?? const [],
);
