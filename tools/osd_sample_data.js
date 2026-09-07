// Pin the sample data sets -- their field mappings, saved objects and where
// their data lives in a distribution -- from the OpenSearch Dashboards source
// tree, to console/sample_data.json. Run from a bootstrapped checkout:
//
//     cd study/OpenSearch-Dashboards
//     node -r ./src/setup_node_env ../../tools/osd_sample_data.js ../../console/sample_data.json
//
// The data itself (flights.json.gz and the others) is read from the
// distribution at run time; only what is code here is pinned.
const fs = require('fs');
const path = require('path');
const sets = require(path.resolve('src/plugins/home/server/services/sample_data/data_sets'));
const out = [];
for (const provider of ['flightsSpecProvider', 'logsSpecProvider', 'ecommerceSpecProvider']) {
  const s = sets[provider]();
  out.push({
    id: s.id,
    name: s.name,
    description: s.description,
    previewImagePath: s.previewImagePath,
    darkPreviewImagePath: s.darkPreviewImagePath,
    hasNewThemeImages: !!s.hasNewThemeImages,
    overviewDashboard: s.overviewDashboard,
    appLinks: s.appLinks,
    defaultIndex: s.defaultIndex,
    dataIndices: s.dataIndices.map((i) => ({
      id: i.id,
      indexName: i.indexName,
      dataPath: 'src/plugins/' + i.dataPath.split('/src/plugins/')[1],
      fields: i.fields,
      timeFields: i.timeFields,
      currentTimeMarker: i.currentTimeMarker,
      preserveDayOfWeekTimeOfDay: !!i.preserveDayOfWeekTimeOfDay,
    })),
    savedObjects: s.savedObjects,
  });
}
fs.writeFileSync(process.argv[2], JSON.stringify(out, null, 1) + '\n');
console.log(out.map((o) => `${o.id}: ${o.savedObjects.length} saved objects, ${o.dataIndices.map((i) => i.dataPath).join(', ')}`).join('\n'));
